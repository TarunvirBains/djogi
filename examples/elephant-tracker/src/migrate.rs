//! Schema management for the example.
//!
//! Adopters in production wire schema management through Djogi's
//! Phase 7 CLI (`djogi migrations compose` / `apply` / `attune`). The
//! example pre-dates that integration and ships a self-contained
//! `migrate` subcommand. Track 0 (sub-step 0.4) routed the bootstrap
//! step through Djogi's canonical bootstrap module so the example's
//! HeeRanjID + PostGIS + node-id GUC install path matches what
//! `db reset` and `migrations apply` use:
//!
//! 1. [`djogi::migrate::bootstrap::run_phase_zero`] installs HeeRanjID,
//!    PostGIS, and the node-id GUC in a single batch — the same code
//!    path adopters and the test harness hit.
//! 2. Drops + recreates every example table through `ctx.raw_*`. (This
//!    is the leftover raw-DDL path Track A will replace with descriptor-
//!    driven `migrations apply`; Track 0 deliberately does NOT touch it.)
//!
//! The function is idempotent — running `migrate` twice is safe and
//! leaves the database in the same state.
//!
//! # Why `pool.raw_with_client` is the bridge
//!
//! `bootstrap::run_phase_zero` takes a `&tokio_postgres::GenericClient`
//! and runs the install batch in one round-trip.
//! `RawPoolAccessExt::raw_with_client` is the
//! documented escape hatch for raw-driver operations — it borrows a
//! `&mut tokio_postgres::Client` for the closure's duration and
//! returns the connection to the pool on `Ok` (or detaches on
//! `Err`/panic to prevent session-state leakage). The inherent
//! `DjogiPool::with_client` method is `pub(crate)`; example/adopter
//! code reaches the same behaviour through the sealed
//! `RawPoolAccessExt` bypass trait, injected by the explicit raw-SQL
//! bypass attribute. The migrate path uses the same pool the rest
//! of the example uses — no one-shot connections, no manual
//! driver-task spawn.

use anyhow::{Context, Result};
use djogi::{DjogiContext, DjogiError};

/// All DDL the example needs, in dependency order.
///
/// Each item is a single statement that `ctx.raw_execute` can handle
/// through prepare-cached. Multi-statement blocks (CREATE TABLE +
/// CREATE INDEX) live in their own `raw_ddl` batches lower down.
const DROP_ORDER: &[&str] = &[
    "DROP TABLE IF EXISTS sightings_outbox CASCADE",
    "DROP TABLE IF EXISTS sightings CASCADE",
    "DROP TABLE IF EXISTS elephant_ancestries CASCADE",
    "DROP TABLE IF EXISTS elephants CASCADE",
    "DROP TABLE IF EXISTS herd_ranges CASCADE",
    "DROP TABLE IF EXISTS herds CASCADE",
    "DROP TABLE IF EXISTS researchers CASCADE",
    "DROP TABLE IF EXISTS countries CASCADE",
];

/// Run the migration. Idempotent.
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): example migration bootstrap uses raw DDL until descriptor-driven example migrations replace it.
pub async fn run(ctx: &mut DjogiContext) -> Result<()> {
    tracing::info!("running Phase 0 bootstrap (HeeRanjID + PostGIS + node-id GUC)");
    install_phase_zero(ctx).await?;

    tracing::info!("dropping existing tables");
    for stmt in DROP_ORDER {
        ctx.raw_execute(stmt, &[])
            .await
            .with_context(|| format!("drop statement failed: {stmt}"))?;
    }

    tracing::info!("creating tables");
    create_tables(ctx).await?;

    tracing::info!("migrate complete");
    Ok(())
}

/// Run Phase 0 bootstrap — HeeRanjID schema + PostGIS extension +
/// node-id GUC seed — via Djogi's canonical
/// [`djogi::migrate::bootstrap::run_phase_zero`].
///
/// Track 0 (sub-step 0.4): pre-Track-0 this function was three
/// separate hand-rolled installs (HeeRanjID schema, desc-support
/// primitives, PostGIS, node-id GUC). Each step independently
/// reproduced what the test harness was doing in
/// `setup_test_db_with_extensions`. Routing through
/// `bootstrap::run_phase_zero` collapses all of that into one call
/// against the SAME bootstrap surface the test harness +
/// `migrations compose` auto-emit + `db reset` replay all use.
///
/// `RawPoolAccessExt::raw_with_client` is the bridge:
/// `bootstrap::run_phase_zero` takes a bare
/// `&tokio_postgres::GenericClient` (since it predates any pool
/// concept and operates outside `DjogiContext`'s ergonomics); the
/// raw bypass borrows the pool's connection and hands it to the
/// closure for the bootstrap's duration. The pool's `post_connect`
/// hook (set in `main.rs`) handles the session-level `SET
/// heer.node_id` for every connection going forward; `bootstrap`
/// adds the database-level `ALTER DATABASE` so freshly-opened pool
/// connections inherit the GUC even before `post_connect` fires.
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): Phase 0 bootstrap requires direct pool/client access for extension and HeeRanjID installation.
async fn install_phase_zero(ctx: &mut DjogiContext) -> Result<()> {
    let pool = ctx
        .raw_pool()
        .ok_or_else(|| anyhow::anyhow!("migrate must be invoked against a pool-backed context"))?
        .clone();

    let database_url =
        std::env::var("DATABASE_URL").context("DATABASE_URL must be set for migrate")?;
    let dbname = parse_database_name(&database_url)
        .context("could not parse database name from DATABASE_URL")?;

    let mut extensions = std::collections::BTreeSet::new();
    extensions.insert("postgis".to_string());

    pool.raw_with_client(|client| {
        Box::pin(async move {
            djogi::migrate::bootstrap::run_phase_zero(
                client,
                &dbname,
                &extensions,
                djogi::migrate::bootstrap::DEFAULT_NODE_ID,
            )
            .await
            .map_err(|e| DjogiError::Validation(format!("phase 0 bootstrap: {e}")))?;
            Ok(())
        })
    })
    .await
    .context("phase 0 bootstrap via pool.raw_with_client")?;
    Ok(())
}

/// Extract the database name (the first path component) from a
/// Postgres connection URL.
///
/// Done by hand — the no-regex rule applies even at the example layer
/// — and limited to the slice of URL syntax the example actually
/// emits. `postgres://<user>:<password>@<host>:<port>/<database>?...` is the only
/// shape we accept.
fn parse_database_name(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    let after_host = after_scheme.split_once('/')?.1;
    let dbname = after_host.split(['?', '#']).next()?;
    if dbname.is_empty() {
        None
    } else {
        Some(dbname.to_string())
    }
}

/// Issue every `CREATE TABLE` + `CREATE INDEX` statement.
///
/// The schema mirrors what Djogi's macro-driven differ would emit for
/// the model declarations in `crate::models`. A real adopter relying
/// on `djogi migrations compose` does not write this by hand.
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): example migration creates its tables with raw DDL until descriptor-driven example migrations land.
async fn create_tables(ctx: &mut DjogiContext) -> Result<()> {
    // Countries — Serial PK, lookup table.
    ctx.raw_ddl(
        "CREATE TABLE countries (
            id          SERIAL      PRIMARY KEY,
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            iso_alpha3  VARCHAR(3)  NOT NULL UNIQUE,
            name        TEXT        NOT NULL
        )",
    )
    .await
    .context("create countries")?;

    // Researchers — HeerId PK, RLS via tenant_key=org_id, FTS on notes.
    // The model-level FTS spec lowers to a GENERATED tsvector column +
    // a GIN index. Tenant-key RLS lowers to ENABLE ROW LEVEL SECURITY
    // plus a CREATE POLICY (Phase 7+ migration concern; the example
    // sets it up but does not exercise the policy below).
    ctx.raw_ddl(
        "CREATE TABLE researchers (
            id           BIGINT      PRIMARY KEY DEFAULT heerid_next(),
            created_at   TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at   TIMESTAMPTZ NOT NULL    DEFAULT now(),
            org_id       BIGINT      NOT NULL,
            name         TEXT        NOT NULL,
            email        TEXT        NOT NULL,
            notes        TEXT        NOT NULL,
            search       TSVECTOR    GENERATED ALWAYS AS (
                             to_tsvector('english', notes)
                         ) STORED
        );
        CREATE INDEX researchers_search_gin
            ON researchers USING GIN (search);",
    )
    .await
    .context("create researchers")?;

    // Herds — HeerId PK. `territory` is the materialised convex-hull
    // polygon over the herd's sightings; populated post-seed by
    // `seed::populate_herd_territories` once the `sightings` rows exist.
    // GiST index on `territory` keeps `ST_Intersection` /
    // `ST_Intersects` predicates index-eligible on the
    // `PairAreaOverlapRatio` mating-pairs scoring path (Phase 8.5 #99).
    ctx.raw_ddl(
        "CREATE TABLE herds (
            id                    BIGINT      PRIMARY KEY DEFAULT heerid_next(),
            created_at            TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at            TIMESTAMPTZ NOT NULL    DEFAULT now(),
            name                  TEXT        NOT NULL UNIQUE,
            estimated_population  INTEGER     NOT NULL,
            territory             GEOGRAPHY(Polygon, 4326)
        );
        CREATE INDEX herds_territory_gix ON herds USING GIST (territory);",
    )
    .await
    .context("create herds")?;

    // HerdRange — explicit-through M2M with `(herd, country, season)`
    // composite uniqueness.
    ctx.raw_ddl(
        "CREATE TABLE herd_ranges (
            id          BIGINT      PRIMARY KEY DEFAULT heerid_next(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            herd_id     BIGINT      NOT NULL    REFERENCES herds(id),
            country_id  INTEGER     NOT NULL    REFERENCES countries(id),
            season      VARCHAR(8)  NOT NULL,
            UNIQUE (herd_id, country_id, season)
        )",
    )
    .await
    .context("create herd_ranges")?;

    // Elephants — HeerId PK, FK to herds, two self-FKs (mother + father)
    // for biological pedigree, typed JSONB tags, optimistic-lock version.
    // The two self-FKs let `Model::materialize_closure` walk both
    // matrilineal and patrilineal chains in one recursive CTE while
    // preserving path multiplicity (load-bearing for Wright kinship).
    // The `mating-pairs` demo reads from the resulting
    // `elephant_ancestries` closure rather than re-walking per
    // query — see the closure-table comment below.
    ctx.raw_ddl(
        "CREATE TABLE elephants (
            id                    BIGINT      PRIMARY KEY DEFAULT heerid_next(),
            created_at            TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at            TIMESTAMPTZ NOT NULL    DEFAULT now(),
            name                  TEXT        NOT NULL,
            herd_id               BIGINT      NOT NULL    REFERENCES herds(id),
            mother_id             BIGINT                  REFERENCES elephants(id),
            father_id             BIGINT                  REFERENCES elephants(id),
            estimated_birth_year  SMALLINT,
            tags                  JSONB       NOT NULL    DEFAULT '{}'::jsonb,
            version               INTEGER     NOT NULL    DEFAULT 0
        );
        CREATE INDEX elephants_herd_id_idx     ON elephants (herd_id);
        CREATE INDEX elephants_mother_id_idx   ON elephants (mother_id);
        CREATE INDEX elephants_father_id_idx   ON elephants (father_id);",
    )
    .await
    .context("create elephants")?;

    // Elephant ancestries — materialized transitive closure of the
    // pedigree graph. Populated post-seed via
    // `Elephant::materialize_closure::<ElephantAncestry>` (Phase 8-Zero
    // Cluster B substrate). The unique constraint on
    // `(elephant_id, ancestor_id, depth)` is load-bearing — the
    // closure helper's `INSERT ... ON CONFLICT (...)` requires it for
    // upsert idempotency.
    ctx.raw_ddl(
        "CREATE TABLE elephant_ancestries (
            id           BIGINT      PRIMARY KEY DEFAULT heerid_next(),
            created_at   TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at   TIMESTAMPTZ NOT NULL    DEFAULT now(),
            elephant_id  BIGINT      NOT NULL    REFERENCES elephants(id) ON DELETE CASCADE,
            ancestor_id  BIGINT      NOT NULL    REFERENCES elephants(id) ON DELETE CASCADE,
            depth        INTEGER     NOT NULL,
            path_count   BIGINT      NOT NULL,
            UNIQUE (elephant_id, ancestor_id, depth)
        );
        CREATE INDEX elephant_ancestries_elephant_id_idx ON elephant_ancestries (elephant_id);
        CREATE INDEX elephant_ancestries_ancestor_id_idx ON elephant_ancestries (ancestor_id);",
    )
    .await
    .context("create elephant_ancestries")?;

    // Sightings — spatial point + FTS notes + outbox.
    //
    // `herd_id` is the denormalized FK that lets the mating-pairs
    // demo's typed `Sighting::objects().group_by(|s| s.herd_id())`
    // pre-aggregate convex hulls without traversing
    // `s.elephant().herd_id()`. The column was added to the Sighting
    // struct in #100 but the DDL above missed the addition until
    // Phase 8.5 Cluster A; the post-#100 model definition is the
    // authority. `NOT NULL` matches the `ForeignKey<Herd>` (non-
    // optional) shape on the struct.
    ctx.raw_ddl(
        "CREATE TABLE sightings (
            id              BIGINT      PRIMARY KEY DEFAULT heerid_next(),
            created_at      TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at      TIMESTAMPTZ NOT NULL    DEFAULT now(),
            elephant_id     BIGINT      NOT NULL    REFERENCES elephants(id),
            herd_id         BIGINT      NOT NULL    REFERENCES herds(id),
            observed_by_id  BIGINT      NOT NULL    REFERENCES researchers(id),
            location        GEOGRAPHY(Point, 4326) NOT NULL,
            observed_at     TIMESTAMPTZ NOT NULL,
            notes           TEXT        NOT NULL,
            search          TSVECTOR    GENERATED ALWAYS AS (
                                to_tsvector('english', notes)
                            ) STORED
        );
        CREATE INDEX sightings_location_gix    ON sightings USING GIST (location);
        CREATE INDEX sightings_search_gin      ON sightings USING GIN (search);
        CREATE INDEX sightings_elephant_id_idx ON sightings (elephant_id);
        CREATE INDEX sightings_herd_id_idx     ON sightings (herd_id);",
    )
    .await
    .context("create sightings")?;

    // Outbox companion for sightings (the `events` flag).
    ctx.raw_ddl(
        "CREATE TABLE sightings_outbox (
            id          BIGINT      PRIMARY KEY DEFAULT heerid_next(),
            row_id      BIGINT      NOT NULL,
            action      TEXT        NOT NULL,
            payload     JSONB       NOT NULL,
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now()
        )",
    )
    .await
    .context("create sightings_outbox")?;

    Ok(())
}
