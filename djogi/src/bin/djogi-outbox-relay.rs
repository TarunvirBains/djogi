//! Reference outbox relay binary —.
//! Polls one or more outbox tables, claims pending rows, publishes them via a
//! configured [`Publisher`], and marks them published or failed. Stale
//! `processing` rows (abandoned by crashed workers) are recovered back to
//! `pending` on each iteration.
//! # Compile requirements
//! This binary requires `features = ["outbox"]` — see `required-features` in
//! `djogi/Cargo.toml`. It is built with:
//! ```shell
//! cargo build -p djogi --features outbox --bin djogi-outbox-relay
//! ```
//! # Configuration
//! | Env var | Default | Description |
//! |---------|---------|-------------|
//! | `DATABASE_URL` | required | Postgres connection URL |
//! | `OUTBOX_TABLES` | `worker_outbox` | Comma-separated table names to poll |
//! | `OUTBOX_BATCH_SIZE` | `32` | Rows to claim per table per iteration |
//! | `OUTBOX_CHANNEL_PREFIX` | `djogi_outbox_` | NOTIFY channel prefix |
//! | `OUTBOX_POLL_MS` | `500` | Milliseconds between poll iterations |
//! # Runtime behaviour
//! Each iteration:
//! 1. For every configured outbox table, opens an atomic scope and claims a
//! batch of pending rows.
//! 2. Publishes each row via the configured publisher.
//! 3. Marks each row published (success) or failed (error).
//! 4. After the atomic scope, calls `recover_stale` outside any transaction
//! to reset rows whose leases have expired.
//! 5. Sleeps for `OUTBOX_POLL_MS` before the next iteration.
//! # Note on long-running loop
//! This binary is intentionally a reference implementation. Production
//! deployments may want to replace the fixed `OUTBOX_POLL_MS` sleep with an
//! adaptive strategy (e.g. exponential backoff when no rows are found,
//! immediate retry when a full batch was claimed). The loop here is kept simple
//! to illustrate the primitives.

use djogi::context::DjogiContext;
use djogi::outbox::Publisher as _;
use djogi::outbox::publishers::NotifyPublisher;
use djogi::outbox::worker;
use djogi::pg::pool::DjogiPool;
use djogi::transaction::atomic;
use time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // -------------------------------------------------------------------------
    // Configuration from environment.
    // -------------------------------------------------------------------------

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL env var is required");

    let outbox_tables: Vec<String> = std::env::var("OUTBOX_TABLES")
        .unwrap_or_else(|_| "worker_outbox".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    let batch_size: u32 = std::env::var("OUTBOX_BATCH_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);

    let channel_prefix =
        std::env::var("OUTBOX_CHANNEL_PREFIX").unwrap_or_else(|_| "djogi_outbox_".to_string());

    let poll_ms: u64 = std::env::var("OUTBOX_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);

    // -------------------------------------------------------------------------
    // Connect.
    // -------------------------------------------------------------------------

    let pool = DjogiPool::connect(&database_url).await?;

    // One NotifyPublisher per outbox table so the NOTIFY channel encodes the
    // source table in its name. Listeners can subscribe to `{channel_prefix}
    // {table}` and receive only the rows from that outbox. This matches the
    // Plan's per-table routing intent.
    let publishers: std::collections::HashMap<String, std::sync::Arc<NotifyPublisher>> =
        outbox_tables
            .iter()
            .map(|t| {
                let channel = format!("{channel_prefix}{t}");
                let publisher = std::sync::Arc::new(NotifyPublisher::new(pool.clone(), channel));
                (t.clone(), publisher)
            })
            .collect();

    // Lease: 5 minutes. Workers must call mark_published/mark_failed within
    // this window or recover_stale will reclaim the row.
    let lease_duration = Duration::minutes(5);

    // Stale threshold: 10 minutes. Rows whose leased_until is more than
    // 10 minutes in the past are considered abandoned.
    let stale_threshold = Duration::minutes(10);

    // -------------------------------------------------------------------------
    // Poll loop.
    // -------------------------------------------------------------------------

    loop {
        for table in &outbox_tables {
            // Look up the per-table publisher and clone the Arc/String so the
            // closure can capture owned values.
            let publisher_arc = std::sync::Arc::clone(
                publishers
                    .get(table)
                    .expect("publisher built for every configured table"),
            );
            let table_owned = table.clone();

            // Claim + publish inside an atomic scope so the state transitions
            // are atomic. If the relay crashes mid-batch the rows stay in
            // 'processing' until recover_stale reclaims them.
            let mut ctx = DjogiContext::from_pool(pool.clone());
            let result = atomic(&mut ctx, |ctx| {
                let publisher_inner = std::sync::Arc::clone(&publisher_arc);
                let tbl = table_owned.clone();
                Box::pin(async move {
                    let rows = worker::claim_pending(ctx, &tbl, batch_size, lease_duration).await?;

                    for row in rows {
                        let row_id = row.id;
                        match publisher_inner.publish(&row).await {
                            Ok(()) => {
                                worker::mark_published(ctx, &tbl, row_id).await?;
                            }
                            Err(djogi::outbox::PublishError::Transient { message }) => {
                                worker::mark_failed(ctx, &tbl, row_id, &message, true).await?;
                            }
                            Err(djogi::outbox::PublishError::Permanent { message }) => {
                                worker::mark_failed(ctx, &tbl, row_id, &message, false).await?;
                            }
                            // Provider errors have unknown transience. Per
                            // `PublishError::Provider`'s rustdoc, the relay
                            // treats them as transient by default so a pool
                            // hiccup or connection drop does not terminally
                            // fail the row. The retry budget
                            // (`MAX_RETRY_COUNT`) is what bounds pathological
                            // cases.
                            Err(e) => {
                                let msg = e.to_string();
                                worker::mark_failed(ctx, &tbl, row_id, &msg, true).await?;
                            }
                        }
                    }

                    Ok::<_, djogi::DjogiError>(())
                })
            })
            .await;

            if let Err(e) = result {
                eprintln!("relay: error processing {table}: {e}");
            }

            // Recover stale rows outside any transaction.
            let mut stale_ctx = DjogiContext::from_pool(pool.clone());
            match worker::recover_stale(&mut stale_ctx, table, stale_threshold).await {
                Ok(0) => {}
                Ok(n) => eprintln!("relay: recovered {n} stale rows from {table}"),
                Err(e) => eprintln!("relay: recover_stale error for {table}: {e}"),
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
    }
}
