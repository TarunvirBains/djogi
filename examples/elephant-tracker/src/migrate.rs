//! Schema management for the example.
//!
//! Adopters in production wire schema management through Djogi's
//! Phase 7 CLI (`djogi migrations compose` / `apply` / `attune`). The
//! example pre-dates that integration and ships a self-contained
//! `migrate` subcommand that:
//!
//! 1. Installs the HeeRanjID schema (`generate_id`, the node table)
//!    if missing — via `pool.with_client(...)` because `heeranjid`'s
//!    installers take a bare `&tokio_postgres::Client`.
//! 2. Seeds node id 1 if missing.
//! 3. Sets `heer.node_id = '1'` at the database level when the
//!    connecting role owns the database. Belt-and-braces alongside
//!    the pool's `post_connect` hook (in `main.rs`), which sets the
//!    same GUC at the session level.
//! 4. Installs the PostGIS extension via the same `with_client` borrow.
//! 5. Drops + recreates every example table through `ctx.raw_*`.
//!
//! The function is idempotent — running `migrate` twice is safe and
//! leaves the database in the same state.
//!
//! # Phase 8-Zero adoption
//!
//! The pre-Phase-8 version of this file opened a side-channel
//! `tokio_postgres::connect` because the framework's pool's `get()`
//! was `pub(crate)` — there was no way to reach the underlying
//! `tokio_postgres::Client` for `heeranjid::install_schema` /
//! `CREATE EXTENSION postgis`. Phase 8-Zero T4 adds
//! `DjogiPool::with_client`, which is exactly that escape hatch.
//! The migrate path now uses the same pool the rest of the example
//! uses — no one-shot connections, no manual driver-task spawn.

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
    "DROP TABLE IF EXISTS elephants CASCADE",
    "DROP TABLE IF EXISTS herd_ranges CASCADE",
    "DROP TABLE IF EXISTS herds CASCADE",
    "DROP TABLE IF EXISTS researchers CASCADE",
    "DROP TABLE IF EXISTS countries CASCADE",
];

/// Run the migration. Idempotent.
pub async fn run(ctx: &mut DjogiContext) -> Result<()> {
    tracing::info!("installing HeeRanjID schema (idempotent)");
    install_heeranjid(ctx).await?;

    tracing::info!("installing PostGIS extension (idempotent) via with_client");
    install_postgis(ctx).await?;

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

/// Install the HeeRanjID schema and seed node id 1.
///
/// `heeranjid::postgres_schema::install_*` and `seed_default_node`
/// take a bare `&tokio_postgres::Client` — they pre-date Djogi and
/// know nothing about `DjogiContext`. Phase 8-Zero T4 introduced
/// `DjogiPool::with_client` as the explicit escape hatch for exactly
/// this case: hand the closure a `&mut Client` for the duration of
/// its body, then return the connection to the pool when the closure
/// resolves cleanly (or detach it on `Err` / panic / cancel — see
/// the `with_client` rustdoc for the full lifecycle).
///
/// We use `with_client` here rather than opening a side-channel
/// `tokio_postgres::connect`. The benefit is that the migrate path
/// participates in the same pool as the rest of the example (a single
/// physical connection serves the whole migrate run on the default
/// `max_size = 5` pool), and the pool's `post_connect` hook (set in
/// `main.rs`) has already pinned `heer.node_id` on the session before
/// `heeranjid::install_schema` runs.
async fn install_heeranjid(ctx: &mut DjogiContext) -> Result<()> {
    let pool = ctx
        .pool()
        .ok_or_else(|| anyhow::anyhow!("migrate must be invoked against a pool-backed context"))?
        .clone();

    pool.with_client(|client| {
        Box::pin(async move {
            heeranjid::postgres_schema::install_schema(client)
                .await
                .map_err(|e| DjogiError::Validation(format!("heeranjid install_schema: {e}")))?;
            heeranjid::postgres_schema::install_all_desc_support(client)
                .await
                .map_err(|e| {
                    DjogiError::Validation(format!("heeranjid install_all_desc_support: {e}"))
                })?;
            heeranjid::postgres_schema::seed_default_node(client)
                .await
                .map_err(|e| DjogiError::Validation(format!("heeranjid seed_default_node: {e}")))?;
            Ok(())
        })
    })
    .await
    .context("heeranjid install via pool.with_client")?;

    // Belt-and-braces: pin `heer.node_id` at the database level too,
    // when we have permission. Combined with the session-level set
    // from `main.rs`'s `post_connect` hook this means a freshly-opened
    // pool connection sees the node id even before any
    // application-level SET runs (some heeranjid functions read the
    // GUC during their first invocation rather than via SET-LOCAL).
    //
    // Requires the connecting role to own the database; the example's
    // dev setup (`djogi:djogi@localhost`) satisfies that. CI / shared
    // databases without ownership rely on the session-level
    // `post_connect` set instead.
    let database_url =
        std::env::var("DATABASE_URL").context("DATABASE_URL must be set for migrate")?;
    let dbname = parse_database_name(&database_url)
        .context("could not parse database name from DATABASE_URL")?;
    let node_id = std::env::var("HEER_NODE_ID").unwrap_or_else(|_| "1".to_string());
    let alter = format!(
        "ALTER DATABASE \"{}\" SET heer.node_id = '{}'",
        dbname, node_id
    );
    if let Err(e) = ctx.raw_ddl(&alter).await {
        // Soft-warn: the example runs fine without the database-level
        // pin as long as the session-level `post_connect` ran. This
        // is the path a CI sandbox without `OWNER` will take.
        tracing::warn!(
            "skipped database-level `heer.node_id` pin (likely missing OWNER privilege): {e}"
        );
    }

    Ok(())
}

/// Install PostGIS via `pool.with_client`. PostGIS' `CREATE EXTENSION`
/// works through Djogi's `ctx.raw_ddl` too, but we route it through
/// `with_client` here as a small worked example of the escape hatch
/// — the rustdoc on `DjogiPool::with_client` calls out
/// `CREATE EXTENSION` specifically as a use case alongside `COPY` and
/// server-side cursors.
async fn install_postgis(ctx: &mut DjogiContext) -> Result<()> {
    let pool = ctx
        .pool()
        .ok_or_else(|| anyhow::anyhow!("migrate must be invoked against a pool-backed context"))?
        .clone();
    pool.with_client(|client| {
        Box::pin(async move {
            client
                .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis")
                .await
                .map_err(DjogiError::from)
        })
    })
    .await
    .context("install postgis extension via with_client")?;
    Ok(())
}

/// Extract the database name (the first path component) from a
/// Postgres connection URL.
///
/// Done by hand — the no-regex rule applies even at the example layer
/// — and limited to the slice of URL syntax the example actually
/// emits. `postgres://user:pass@host:port/dbname?...` is the only
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
            id           BIGINT      PRIMARY KEY DEFAULT generate_id(),
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

    // Herds — HeerId PK.
    ctx.raw_ddl(
        "CREATE TABLE herds (
            id                    BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at            TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at            TIMESTAMPTZ NOT NULL    DEFAULT now(),
            name                  TEXT        NOT NULL UNIQUE,
            estimated_population  INTEGER     NOT NULL
        )",
    )
    .await
    .context("create herds")?;

    // HerdRange — explicit-through M2M with `(herd, country, season)`
    // composite uniqueness.
    ctx.raw_ddl(
        "CREATE TABLE herd_ranges (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
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

    // Elephants — HeerId PK, FK to herds, self-FK for parent lineage,
    // typed JSONB tags, optimistic-lock version.
    ctx.raw_ddl(
        "CREATE TABLE elephants (
            id                    BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at            TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at            TIMESTAMPTZ NOT NULL    DEFAULT now(),
            name                  TEXT        NOT NULL,
            herd_id               BIGINT      NOT NULL    REFERENCES herds(id),
            parent_id             BIGINT                  REFERENCES elephants(id),
            estimated_birth_year  SMALLINT,
            tags                  JSONB       NOT NULL    DEFAULT '{}'::jsonb,
            version               INTEGER     NOT NULL    DEFAULT 0
        );
        CREATE INDEX elephants_herd_id_idx     ON elephants (herd_id);
        CREATE INDEX elephants_parent_id_idx   ON elephants (parent_id);",
    )
    .await
    .context("create elephants")?;

    // Sightings — spatial point + FTS notes + outbox.
    ctx.raw_ddl(
        "CREATE TABLE sightings (
            id              BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at      TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at      TIMESTAMPTZ NOT NULL    DEFAULT now(),
            elephant_id     BIGINT      NOT NULL    REFERENCES elephants(id),
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
        CREATE INDEX sightings_elephant_id_idx ON sightings (elephant_id);",
    )
    .await
    .context("create sightings")?;

    // Outbox companion for sightings (the `events` flag).
    ctx.raw_ddl(
        "CREATE TABLE sightings_outbox (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
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
