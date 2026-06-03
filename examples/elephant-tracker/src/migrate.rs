//! Schema management for the example.
//!
//! Djogi emits every `CREATE TABLE` / `CREATE INDEX` statement from the
//! `#[model(...)]` descriptors through the library migration pipeline, so
//! this module does not hand-write any DDL for the example's application
//! tables. Two raw-DDL paths remain:
//!
//! 1. **Phase 0 bootstrap** (`install_phase_zero`) — installs HeeRanjID
//!    functions and the PostGIS extension. Legitimately raw: no typed
//!    migration surface exists for database-level extension installation.
//! 2. **`drop_all`** — issues one `batch_execute` of `DROP TABLE IF EXISTS
//!    … CASCADE` statements (names derived from the descriptor projection)
//!    plus the migration ledger. This is a deliberate dev-mode wipe; no
//!    typed drop surface exists yet.
//!
//! # How it works
//!
//! `migrate` runs four steps:
//!
//! 1. **Phase 0 bootstrap** — installs HeeRanjID SQL functions + PostGIS
//!    through `batch_execute`. Idempotent (`CREATE OR REPLACE`,
//!    `CREATE EXTENSION IF NOT EXISTS`). Per-connection session GUCs are
//!    set in `main.rs` `post_connect`.
//!
//! 2. **Project inventory** — calls `djogi::migrate::project_from_inventory()`
//!    which iterates the link-time `inventory::iter::<ModelDescriptor>()`
//!    collector and projects each descriptor into an `AppliedSchema` map.
//!    Same call path that `djogi migrations compose` uses.
//!
//! 3. **Lock + Drop** — acquires the workspace file lock on
//!    `.djogi-migrations-lock` in the process working directory, then drops
//!    every projected table (names from descriptors, not hardcoded) plus the
//!    migration ledger in a single `batch_execute`. `CASCADE` handles FK
//!    ordering. `apply_plan` bootstraps a fresh ledger in step 4.
//!
//! 4. **Apply** — diffs the inventory projection against an empty baseline
//!    (all tables were just dropped), plans the delta into a
//!    `MigrationPlan`, and calls `djogi::migrate::apply_plan`. The runner
//!    bootstraps `djogi_schema_migrations`, acquires the per-bucket Postgres
//!    advisory lock, and executes every `CREATE TABLE` + `CREATE INDEX`
//!    statement in the plan — the same SQL that `djogi migrations compose`
//!    would write to a pending migration file.
//!
//! The workspace lock is acquired before the destructive drop in step 3 and
//! held through step 4, so a concurrent `migrate` invocation blocks on
//! `acquire_workspace_lock` rather than racing the drop.
//!
//! # Re-running
//!
//! Running `migrate` again produces a fresh schema and a new ledger row (the
//! version string embeds the current UTC timestamp). The prior schema is
//! destroyed and any data written between runs is lost. This is intentional
//! for a dev-mode example — it is not the same as an idempotent migration
//! that leaves database state unchanged.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use time::OffsetDateTime;

use djogi::DjogiContext;
use djogi::config::MigrateConfig;
use djogi::migrate::{
    AppliedSchema, BucketKey, GUARD_DEFAULT_TIMEOUT, LOCK_FILE_NAME, OutOfOrderPolicy, RunnerCtx,
    WorkspaceGuard, acquire_workspace_lock, apply_plan, compute_checksum, diff_bucket_maps,
    plan_delta, project_from_inventory, version_id, version_prefix,
};

/// Run the migration.
///
/// Drops the existing schema and recreates it from the model descriptors.
/// The workspace file lock is acquired before the destructive drop and held
/// through schema apply, so concurrent invocations block rather than race.
/// Each run produces a fresh schema with a new ledger row; prior row data is
/// lost.
pub async fn run(ctx: &mut DjogiContext) -> Result<()> {
    tracing::info!("running Phase 0 bootstrap (HeeRanjID + PostGIS + node-id GUC)");
    install_phase_zero(ctx).await?;

    // Project the inventory → target AppliedSchema. Table names in the drop
    // step come from this map, not from a hardcoded list.
    let after =
        project_from_inventory().context("projecting model descriptors into AppliedSchema")?;

    // Acquire the workspace file lock before the destructive drop and hold it
    // through schema apply. Two concurrent `migrate` invocations must not both
    // enter the drop+apply critical section simultaneously.
    let workspace_root =
        std::env::current_dir().context("cannot determine workspace root for migration lock")?;
    let lock_path = workspace_root.join(LOCK_FILE_NAME);
    let guard: WorkspaceGuard = acquire_workspace_lock(&lock_path, GUARD_DEFAULT_TIMEOUT)
        .context("acquiring workspace migration lock")?;

    tracing::info!("dropping existing tables");
    drop_all(ctx, &after).await?;

    tracing::info!("applying descriptor-driven schema");
    apply_descriptor_schema(ctx, &after, &guard).await?;

    tracing::info!("migrate complete");
    Ok(())
}

/// Run Phase 0 bootstrap — HeeRanjID schema/default-node seed plus
/// PostGIS extension — through the example's pool.
///
/// The production/test bootstrap surface still owns canonical Phase 0
/// composition. This example deliberately uses
/// `phase_zero_sql_without_database_guc()` instead of
/// `bootstrap::run_phase_zero` so it avoids the database-level
/// `ALTER DATABASE ... SET ...` part; runnable examples should work for
/// roles that can create schema objects and extensions in a sandbox but
/// do not own the database. The pool's `post_connect` hook in `main.rs`
/// is the public per-connection setup surface and sets both HeeRanjID
/// GUCs for every connection.
///
/// `DjogiContext::raw_ddl` is the bridge:
/// `phase_zero_sql_without_database_guc()` builds a raw SQL batch and
/// `ctx.raw_ddl` executes it through the example's allowed raw-DDL helper path.
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): local no-ALTER-DATABASE Phase 0 bootstrap requires
// direct pool/client access for extension and HeeRanjID installation.
async fn install_phase_zero(ctx: &mut DjogiContext) -> Result<()> {
    ctx.raw_ddl(&phase_zero_sql_without_database_guc())
        .await
        .context("phase 0 bootstrap via ctx.raw_ddl")?;
    Ok(())
}

fn phase_zero_sql_without_database_guc() -> String {
    let mut sql = String::with_capacity(
        heeranjid::postgres_schema::INSTALL_SQL.len()
            + heeranjid::postgres_schema::DESC_FLIP_SQL.len()
            + heeranjid::postgres_schema::DESC_GENERATORS_SQL.len()
            + heeranjid::postgres_schema::BULK_BACKFILL_SQL.len()
            + heeranjid::postgres_schema::SEED_SQL.len()
            + 512,
    );
    sql.push_str("-- HeeRanjID base schema + functions (idempotent).\n");
    sql.push_str(heeranjid::postgres_schema::INSTALL_SQL);
    sql.push_str("\n\n-- HeeRanjID desc-flip primitives.\n");
    sql.push_str(heeranjid::postgres_schema::DESC_FLIP_SQL);
    sql.push_str("\n\n-- HeeRanjID single-row generators.\n");
    sql.push_str(heeranjid::postgres_schema::DESC_GENERATORS_SQL);
    sql.push_str("\n\n-- HeeRanjID migration-support procedures.\n");
    sql.push_str(heeranjid::postgres_schema::BULK_BACKFILL_SQL);
    sql.push_str("\n\n-- HeeRanjID default-node seed.\n");
    sql.push_str(heeranjid::postgres_schema::SEED_SQL);
    sql.push_str("\n\n-- PostGIS required by elephant-tracker spatial fields.\n");
    sql.push_str("CREATE EXTENSION IF NOT EXISTS postgis;\n");
    sql
}

/// Drop all projected tables and the migration ledger in a single
/// `batch_execute`.
///
/// Table names come from the descriptor projection — the same source the
/// apply step uses — so no name is hardcoded. `CASCADE` handles FK
/// ordering. The ledger (`djogi_schema_migrations`) is dropped first so
/// `apply_plan` bootstraps a fresh one in the next step.
///
/// All `DROP` statements are batched into a single `raw_ddl` call so a
/// failure on any statement is surfaced as one error rather than leaving
/// the schema in a partially-dropped state.
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): dev-mode wipe before fresh apply. Table names
// are derived from the descriptor projection; raw_ddl (batch_execute) is
// the only available drop surface on DjogiContext. All DROP statements are
// collapsed into one call to avoid partial-drop failure modes.
async fn drop_all(
    ctx: &mut DjogiContext,
    after: &BTreeMap<BucketKey, AppliedSchema>,
) -> Result<()> {
    // Build all DROP statements into a single batch. Sending them as one
    // batch_execute call avoids the partial-drop risk of a per-statement loop.
    let mut sql = String::new();
    // Drop the migration ledger first so apply_plan bootstraps a fresh one.
    sql.push_str("DROP TABLE IF EXISTS djogi_schema_migrations CASCADE;\n");
    // Drop all projected tables. Names come from the descriptor projection.
    for schema in after.values() {
        for table_name in schema.models.keys() {
            sql.push_str(&format!("DROP TABLE IF EXISTS \"{table_name}\" CASCADE;\n"));
        }
    }
    ctx.raw_ddl(&sql)
        .await
        .context("dropping tables and migration ledger")?;
    Ok(())
}

/// Apply the example's full table schema through the framework's descriptor
/// pipeline rather than hand-written DDL.
///
/// Steps mirror what `djogi migrations compose` + `djogi migrations apply`
/// does in a production adopter project:
///
/// 1. Diff the inventory projection against an empty baseline (all tables
///    were just dropped) to produce a pure-additive `SchemaDelta`.
/// 2. Group into a `MigrationPlan` with checksummed, ordered segments.
/// 3. Apply through the runner — same code path as `djogi migrations apply`.
///
/// The caller holds the workspace file lock for the entire duration of this
/// function (lock acquired in `run()` before `drop_all`). `guard` is passed
/// through to `apply_plan` so the runner can verify lock ownership.
///
/// `snapshot` is `None`: the example re-derives the plan from empty on
/// every `migrate` run, so there is no cumulative snapshot drift to track.
/// Production adopters persist the snapshot so the differ can track
/// incremental changes; the example's drop-and-recreate model makes that
/// unnecessary here.
async fn apply_descriptor_schema(
    ctx: &mut DjogiContext,
    after: &BTreeMap<BucketKey, AppliedSchema>,
    guard: &WorkspaceGuard,
) -> Result<()> {
    // The snapshot read (empty baseline) and diff both run inside the lock
    // window established by the caller, so there is no TOCTOU gap between
    // snapshot read and lock acquisition.
    let before: BTreeMap<BucketKey, AppliedSchema> = BTreeMap::new();

    let deltas = diff_bucket_maps(&before, after).context("computing schema delta")?;

    let version = version_id(&version_prefix(OffsetDateTime::now_utc()), "initial_schema");

    for delta in &deltas {
        let plan = plan_delta(delta).context("planning schema delta")?;
        if plan.segments.is_empty() {
            continue;
        }

        // Compute checksums from the ordered segment SQL — same format the
        // compose pipeline writes to pending migration files.
        let up_frags: Vec<&str> = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter().map(|op| op.up.as_str()))
            .collect();
        let down_frags: Vec<&str> = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter().map(|op| op.down.as_str()))
            .collect();
        let checksum_up = compute_checksum(up_frags.iter().copied());
        // AddTable/AddIndex operations have real DROP SQL on the down side.
        let checksum_down = Some(compute_checksum(down_frags.iter().copied()));

        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: version.clone(),
            description: "initial schema — descriptor-driven".to_string(),
            checksum_up,
            checksum_down,
            // No snapshot persistence — the example drops and recreates on
            // every `migrate` run; snapshot drift tracking is unnecessary here.
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None, // example does not use runner identity
        };

        apply_plan(ctx, &plan, &runner_ctx, guard)
            .await
            .with_context(|| {
                format!(
                    "applying schema for bucket (database={}, app={})",
                    runner_ctx.bucket.database, runner_ctx.bucket.app
                )
            })?;
    }

    Ok(())
}
