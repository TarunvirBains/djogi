//! In-process model-event NOTIFY subscription surface.
//! `subscribe::<M>(pool)` returns a `TypedReceiver<M>` that fires whenever
//! a row in `M::table_name` is created, saved, or deleted by any
//! `DjogiContext` configured against the same Postgres database.
//! Behind `feature = "notify"`. Companion to the publisher hook in
//! `crate::outbox::emit_event`: every `#[model(events)]` write fires
//! `pg_notify('djogi_<table>', '{"kind":"<action>","id":"<pk>"}')` inside
//! the parent transaction. Subscribers decode that payload and surface
//! `ModelEvent<M> { kind, id }`. Adopters re-fetch the full row via
//! `M::find(...)` when they need the columns; the slim id-only payload
//! sidesteps the 8000-byte `pg_notify` cap.
//! # Lifecycle
//! The strong-reference contract: subscribers hold the listener alive,
//! the registry watches without prolonging life. Four drop paths fall
//! out of that:
//! 1. **Subscriber drop.** The receiver slot is returned to the broadcast
//! channel. The listener and per-channel `Sender` stay up for other
//! subscribers.
//! 2. **Last-subscriber drop.** The listener's strong count hits zero,
//! its dedicated `tokio_postgres::Client` drops, the spawned
//! connection-watcher observes `poll_message` ending and exits. The
//! registry's `Weak` entry becomes dangling and is reaped lazily on
//! the next `subscribe` against the same `pool_id`.
//! 3. **Hot reload.** A freshly-built `DjogiPool` gets a fresh `pool_id`,
//! so `subscribe` against the new pool always misses the registry
//! and spawns a fresh listener.
//! 4. **Watcher failure (GH#131).** The watcher task exits while
//! subscribers are still alive (Postgres terminated our backend, the
//! socket dropped, the senders mutex was poisoned, or the watcher
//! panicked). A `WatcherExitGuard` owned by the spawned task fires
//! on every exit path — clearing the senders map (so live
//! `TypedReceiver`s wake with `RecvError::Closed` rather than
//! blocking forever) and publishing a `failed` flag (so the next
//! `subscribe` reaps the dead registry slot and spawns a fresh
//! listener). Adopters surface
//! [`NotifyError::ListenerTerminated`] from `recv` and recover by
//! re-subscribing.
//! # Wire schema
//! Stable JSON shape on `djogi_<M::TABLE>` channels:
//! ```json
//! { "kind": "create" | "save" | "delete", "id": "<M::Pk Display>" }
//! ```
//! `kind` strings exactly match `OutboxAction::as_sql_str` — any
//! schema bump goes through that const for forward-compat.

use crate::pg::pool::DjogiPool;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::broadcast;
use tokio_postgres::AsyncMessage;

// ── Public surface types ─────────────────────────────────────────────────────

/// Event kinds carried by `ModelEvent<M>`.
/// `OutboxAction::Save` surfaces as `EventKind::Updated`; the wire payload
/// still says `"save"` (matching the outbox `action` column). The rename
/// is a Rust-side ergonomic — `event.kind == EventKind::Updated` reads
/// naturally without outbox-ledger context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// Source row was newly inserted. Wire `"create"`.
    Created,
    /// Source row was updated in place. Wire `"save"`.
    Updated,
    /// Source row was hard-deleted. Wire `"delete"`.
    Deleted,
}

/// Decoded notification event for model `M`.
/// Carries only the row's primary key — adopters re-fetch the full row
/// via `M::find(ctx, event.id).await?` when they need the columns. The
/// id-only payload sidesteps the 8000-byte `pg_notify` cap.
#[derive(Debug, Clone)]
pub struct ModelEvent<M: crate::model::Model> {
    /// What happened to the row.
    pub kind: EventKind,
    /// PK of the affected row, decoded from the wire `id` string via
    /// `M::Pk::from_str`.
    pub id: M::Pk,
}

/// Errors surfaced by the notify subscriber surface.
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    /// The broadcast channel dropped some events because a slow
    /// subscriber fell behind. The integer is the number of events
    /// skipped — adopters can decide whether to re-fetch full state
    /// or ignore the gap.
    #[error("broadcast channel lagged by {skipped} events")]
    ChannelLagged { skipped: u64 },

    /// Failed to acquire a connection from the pool to start the
    /// LISTEN background task.
    #[error("failed to start NOTIFY listener: {0}")]
    ListenerStartFailed(String),

    /// Wire payload could not be decoded as `{kind, id}`. The raw
    /// string is preserved for diagnostics; the source error is the
    /// underlying serde_json failure.
    #[error("payload decode failed for {raw:?}: {source}")]
    PayloadDecode {
        raw: String,
        #[source]
        source: serde_json::Error,
    },

    /// `M::Pk::from_str` failed on the decoded id string.
    #[error("invalid id {id:?} in payload (M::Pk::from_str rejected): {reason}")]
    InvalidId { id: String, reason: String },

    /// The watcher task that drives this listener has exited. Live
    /// `TypedReceiver::recv` calls surface this when the dedicated
    /// LISTEN connection died abnormally — Postgres terminated our
    /// backend, the underlying socket dropped, the watcher's senders
    /// mutex was poisoned, or the watcher panicked. `subscribe::<M>`
    /// against the same pool detects the failed listener, reaps the
    /// registry slot, and spawns a fresh listener — adopters recover
    /// by re-subscribing.
    /// Distinct from `ListenerStartFailed`: `ListenerStartFailed`
    /// covers spawn-time failures (initial connect / first `LISTEN`),
    /// `ListenerTerminated` covers post-spawn death of an already-
    /// running listener.
    #[error("notify watcher terminated; re-subscribe to start a fresh listener")]
    ListenerTerminated,
}

// ── Internal: per-channel broadcast ──────────────────────────────────────────

/// Untyped event flowing from the listener task to per-model decoders.
/// Carries the raw `(kind_str, id_str)` pair lifted out of the wire
/// payload; per-model `subscribe::<M>` adapters convert to typed
/// `ModelEvent<M>`.
#[derive(Debug, Clone)]
struct RawEvent {
    kind_str: String,
    id_str: String,
}

/// One per-pool listener task. Owns a dedicated standalone
/// `tokio_postgres::Client` for `LISTEN`/`AsyncMessage` polling; the
/// `senders` map is shared via `Arc` with the spawned connection
/// watcher so subscribe-side writes and watcher-side reads land on the
/// same allocation.
struct PgListener {
    senders: Arc<Mutex<HashMap<String, broadcast::Sender<RawEvent>>>>,
    /// Set to `true` by the spawned watcher task's
    /// [`WatcherExitGuard`] when the watcher exits for any reason
    /// (normal stream end, `Err` on `poll_message`, or panic unwind).
    /// Read by [`get_or_start_listener`] to reap a dead listener and
    /// by [`subscribe`] to detect the late-watcher-death race. Closes
    /// the door on the GH#131 hazard where `TypedReceiver`s would
    /// block forever on a dead listener because the registry's
    /// `Arc<PgListener>` was still strong.
    failed: Arc<AtomicBool>,
    /// Keepalive for the LISTEN connection — dropping `client` tears
    /// down the connection task and notifications stop.
    #[allow(dead_code)] // kept-alive guard, not directly accessed after spawn
    client: tokio_postgres::Client,
}

impl PgListener {
    /// Returns `true` once the spawned watcher task has exited (for
    /// any reason). Live `TypedReceiver`s on a failed listener see
    /// `Closed` on `recv` because the watcher's exit guard cleared
    /// the senders map; `subscribe::<M>` against a failed listener
    /// surfaces [`NotifyError::ListenerTerminated`] after reaping the
    /// registry slot.
    fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}

impl Drop for PgListener {
    fn drop(&mut self) {
        // By construction, every `TypedReceiver` holds a strong
        // `Arc<PgListener>`, so reaching `Drop` means no live receivers
        // remain. The watcher task observes the dedicated `client`
        // dropping here (via its connection's `poll_message` returning
        // `None`), exits, and the [`WatcherExitGuard`] then clears the
        // senders map and publishes the failed flag — no observable
        // effect at this point because no receivers are waiting, but
        // it leaves the state consistent for the dangling-`Weak`
        // reaper in [`upgrade_existing`].
        tracing::debug!(
            target: "djogi::notify",
            "PgListener dropped — dedicated client torn down, watcher will exit"
        );
    }
}

/// Clear every per-channel [`broadcast::Sender`] from the map. Each
/// dropped `Sender` ticks its broadcast channel's sender count toward
/// zero; once the last `Sender` for a channel is gone, every
/// `broadcast::Receiver` cloned off it surfaces
/// [`broadcast::error::RecvError::Closed`] on the next `recv` (or
/// `try_recv`).
/// Tolerates a poisoned lock by recovering the inner data via
/// `into_inner`. The map's invariant is just "valid `String` keys
/// and live `Sender` values"; we never partially mutate inside a lock
/// scope that could panic, so the map's structural integrity survives
/// a poison. Treat the abnormal lock state as one more reason to
/// clear and exit, not a fatal cascade — silently propagating a
/// `PoisonError` here would re-panic in the watcher task and bypass
/// the cleanup the [`WatcherExitGuard`] is responsible for.
fn close_all_senders(senders: &Mutex<HashMap<String, broadcast::Sender<RawEvent>>>) {
    let mut guard = match senders.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!(
                target: "djogi::notify",
                "senders mutex was poisoned; clearing on watcher exit anyway"
            );
            poisoned.into_inner()
        }
    };
    guard.clear();
}

/// Drop guard owned by the spawned listener task.
/// Fires on the watcher future's normal and unwind exits: graceful
/// end-of-stream, an `Err` return from `poll_message` followed by a
/// `break`, or a panic unwinding through the task body. Whichever path
/// fires, `Drop` runs, which:
/// 1. Calls [`close_all_senders`] so live broadcast receivers wake
/// with [`broadcast::error::RecvError::Closed`] instead of blocking
/// forever on a dead pump.
/// 2. Publishes the `failed` flag so subsequent
/// [`get_or_start_listener`] calls observe the death, reap the
/// registry slot, and spawn a fresh listener.
/// The pair closes the GH#131 hazard: previously, a watcher that
/// `break`-ed on `poll_message` error left every `broadcast::Sender`
/// alive in the map, so any `TypedReceiver` still keepalived by an
/// adopter saw "no events" rather than `Closed` and could not
/// distinguish a healthy-but-quiet channel from a dead listener.
struct WatcherExitGuard {
    senders: Arc<Mutex<HashMap<String, broadcast::Sender<RawEvent>>>>,
    failed: Arc<AtomicBool>,
}

impl Drop for WatcherExitGuard {
    fn drop(&mut self) {
        // Clear the senders first so any blocked receivers see
        // `Closed`, then publish the failed flag so future
        // subscriptions stop reusing this listener.
        close_all_senders(&self.senders);
        self.failed.store(true, Ordering::Release);
        tracing::debug!(
            target: "djogi::notify",
            "watcher exit guard fired — senders cleared, failed flag published"
        );
    }
}

// ── Internal: per-process pool registry ──────────────────────────────────────

/// Pool-keyed registry of running `PgListener` instances.
/// Keyed by [`DjogiPool::pool_id`] (per-process unique, copied verbatim
/// on `Clone`). Stores `Weak<PgListener>` so the registry never prolongs
/// the listener's life — strong refs live on `TypedReceiver<M>`.
fn registry() -> &'static Mutex<HashMap<u64, Weak<PgListener>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, Weak<PgListener>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pool_key(pool: &DjogiPool) -> u64 {
    pool.pool_id
}

/// Look up `key` in the registry, upgrading the `Weak` if alive.
/// Reaps a dangling slot on a failed upgrade so dead entries don't
/// accumulate across the process lifetime.
fn upgrade_existing<T>(map: &Mutex<HashMap<u64, Weak<T>>>, key: u64) -> Option<Arc<T>> {
    let mut guard = map.lock().expect("notify registry mutex poisoned");
    if let Some(weak) = guard.get(&key) {
        if let Some(strong) = weak.upgrade() {
            return Some(strong);
        }
        guard.remove(&key);
    }
    None
}

/// Remove `key` only if the registry still points at `expected`.
/// Used when a caller observes a failed listener: concurrent
/// subscribers may already have reaped that failed slot and installed a
/// fresh listener. In that race, unconditional `remove(key)` would erase
/// the healthy replacement. Pointer equality makes the cleanup precise:
/// we only remove the same allocation we observed, or a dangling slot.
fn remove_if_current<T>(map: &Mutex<HashMap<u64, Weak<T>>>, key: u64, expected: &Arc<T>) -> bool {
    let mut guard = map.lock().expect("notify registry mutex poisoned");
    let should_remove = match guard.get(&key).and_then(Weak::upgrade) {
        Some(current) => Arc::ptr_eq(&current, expected),
        None => guard.contains_key(&key),
    };
    if should_remove {
        guard.remove(&key);
    }
    should_remove
}

/// Insert `candidate`'s `Weak` under `key`, or return the existing
/// canonical `Arc` if a concurrent first-caller already won. The loser's
/// candidate drops at the call site; only the winner stays up.
fn install_or_lose<T>(map: &Mutex<HashMap<u64, Weak<T>>>, key: u64, candidate: Arc<T>) -> Arc<T> {
    let mut guard = map.lock().expect("notify registry mutex poisoned");
    match guard.get(&key).and_then(|w| w.upgrade()) {
        Some(existing) => existing,
        None => {
            guard.insert(key, Arc::downgrade(&candidate));
            candidate
        }
    }
}

// ── Wire payload decode ──────────────────────────────────────────────────────

/// Decode a `RawEvent` (already split into `kind_str`/`id_str`) into a
/// typed `ModelEvent<M>`. Used by both the unit-test JSON path
/// (`decode_payload`) and the hot-path `recv` so receivers don't
/// re-encode through JSON just to re-parse.
fn decode_event<M: crate::model::Model>(raw: &RawEvent) -> Result<ModelEvent<M>, NotifyError>
where
    M::Pk: FromStr,
    <M::Pk as FromStr>::Err: std::fmt::Display,
{
    let kind = match raw.kind_str.as_str() {
        "create" => EventKind::Created,
        "save" => EventKind::Updated,
        "delete" => EventKind::Deleted,
        other => {
            return Err(NotifyError::PayloadDecode {
                // Reconstruct via `serde_json::json!` (not raw `format!`)
                // so a kind/id containing a quote or backslash still
                // round-trips into valid JSON for diagnostics.
                raw: serde_json::json!({ "kind": other, "id": &raw.id_str }).to_string(),
                source: serde::de::Error::unknown_variant(other, &["create", "save", "delete"]),
            });
        }
    };
    let id = M::Pk::from_str(&raw.id_str).map_err(|e| NotifyError::InvalidId {
        id: raw.id_str.clone(),
        reason: e.to_string(),
    })?;
    Ok(ModelEvent { kind, id })
}

/// Decode a raw NOTIFY payload of shape `{"kind":"...","id":"..."}`
/// into a typed `ModelEvent<M>`. Test-side helper — runtime hot paths
/// already hold the parsed `RawEvent` and use `decode_event` directly.
#[cfg(test)]
fn decode_payload<M: crate::model::Model>(payload: &str) -> Result<ModelEvent<M>, NotifyError>
where
    M::Pk: FromStr,
    <M::Pk as FromStr>::Err: std::fmt::Display,
{
    let raw = parse_raw(payload).map_err(|source| NotifyError::PayloadDecode {
        raw: payload.to_string(),
        source,
    })?;
    decode_event::<M>(&raw)
}

// ── Listener startup + LISTEN management ─────────────────────────────────────

/// Acquire-or-create the `PgListener` for `pool`. Lazy: first call spawns
/// the background task, later calls upgrade the existing `Weak`. Two
/// reaping paths converge here:
/// 1. **Dangling `Weak`** — listener torn down after the last
/// subscriber dropped. [`upgrade_existing`] reaps the slot on its
/// failed `upgrade` and we fall through to spawning fresh.
/// 2. **Failed listener (GH#131)** — the `Arc<PgListener>` is still
/// strong (subscribers alive, keepaliving it) but its watcher task
/// has exited. We see `is_failed == true`, drop our local strong
/// ref, remove the registry slot so concurrent subscribers don't
/// reuse it, and spawn fresh. Old `TypedReceiver`s holding strong
/// refs keep the failed allocation reachable until they drop, but
/// the registry no longer routes new subscriptions to it.
async fn get_or_start_listener(pool: &DjogiPool) -> Result<Arc<PgListener>, NotifyError> {
    let key = pool_key(pool);
    loop {
        if let Some(listener) = upgrade_existing(registry(), key) {
            if !listener.is_failed() {
                return Ok(listener);
            }
            // Failed listener — reap the registry slot only if it
            // still points at this exact allocation. A concurrent
            // resubscriber may have already installed a fresh healthy
            // listener; in that case, retry lookup and reuse it rather
            // than erasing the replacement.
            if remove_if_current(registry(), key, &listener) {
                break;
            }
            continue;
        }
        break;
    }
    let candidate = Arc::new(spawn_listener(pool).await?);
    Ok(install_or_lose(registry(), key, candidate))
}

/// Spawn the per-pool listener task. Uses a standalone
/// `tokio_postgres::connect` rather than the pool because deadpool's
/// `Object` doesn't expose the `Connection` half needed for
/// `AsyncMessage` polling. This is framework substrate code — the
/// `clippy::disallowed_methods` lint that gates direct `tokio_postgres`
/// use against adopters does not apply here, so the allow is local
/// rather than global.
#[allow(clippy::disallowed_methods)]
async fn spawn_listener(pool: &DjogiPool) -> Result<PgListener, NotifyError> {
    let url = pool.url.as_deref().ok_or_else(|| {
        NotifyError::ListenerStartFailed(
            "DjogiPool::url is None — pool was constructed via internal substrate \
             without a URL, so the NOTIFY listener cannot spawn a dedicated \
             connection. Use `DjogiPool::builder(url).build()` for adopter-facing \
             pools."
                .to_string(),
        )
    })?;
    let (client, mut connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .map_err(|e| NotifyError::ListenerStartFailed(e.to_string()))?;

    let senders: Arc<Mutex<HashMap<String, broadcast::Sender<RawEvent>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let failed: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let senders_for_task = Arc::clone(&senders);
    let exit_guard = WatcherExitGuard {
        senders: Arc::clone(&senders),
        failed: Arc::clone(&failed),
    };

    tokio::spawn(async move {
        // Move the exit guard into the task. Drop runs on every exit
        // path this future takes: `break` from poll_message error,
        // graceful stream end, or panic unwind through the body. Drop
        // clears the senders map (so live receivers see `Closed`) and
        // publishes the `failed` flag (so new subscribers reap and
        // respawn). Closes the GH#131 hazard.
        let _exit_guard = exit_guard;

        use futures::StreamExt;
        let mut stream = futures::stream::poll_fn(move |cx| connection.poll_message(cx));
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(AsyncMessage::Notification(n)) => {
                    let channel = n.channel();
                    let payload = n.payload();
                    let raw = match parse_raw(payload) {
                        Ok(r) => r,
                        Err(_) => {
                            tracing::warn!(
                                target: "djogi::notify",
                                channel = %channel,
                                payload = %payload,
                                "discarded malformed notify payload"
                            );
                            continue;
                        }
                    };
                    // Tolerate a poisoned senders mutex by exiting the
                    // watcher loop rather than re-panicking. The exit
                    // guard's drop clears the map (recovering from the
                    // poison via `into_inner` in `close_all_senders`)
                    // and publishes `failed`. Subscribers see
                    // `ListenerTerminated` and re-subscribe spawns a
                    // fresh listener — graceful degradation rather
                    // than a permanently wedged process. Closes the
                    // GH#131 path-1 hazard (panic in senders lookup).
                    let senders_guard = match senders_for_task.lock() {
                        Ok(guard) => guard,
                        Err(_poisoned) => {
                            tracing::error!(
                                target: "djogi::notify",
                                "senders mutex poisoned mid-watch; exiting watcher \
                                 (exit guard will clear and publish failed)"
                            );
                            break;
                        }
                    };
                    if let Some(tx) = senders_guard.get(channel) {
                        let _ = tx.send(raw);
                    }
                }
                Ok(AsyncMessage::Notice(n)) => {
                    tracing::debug!(target: "djogi::notify", "postgres notice: {n}");
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(
                        target: "djogi::notify",
                        error = %e,
                        "notify connection terminated; subscribers will see \
                         ListenerTerminated on next recv()"
                    );
                    break;
                }
            }
        }
        // Falling out of the while loop drops `_exit_guard`, which
        // clears the senders map and publishes the failed flag.
    });

    Ok(PgListener {
        senders,
        failed,
        client,
    })
}

/// Lift the wire payload into the untyped (kind_str, id_str) pair the
/// listener task forwards over the broadcast channel.
fn parse_raw(payload: &str) -> Result<RawEvent, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_str(payload)?;
    let kind_str = v["kind"]
        .as_str()
        .ok_or_else(|| serde::de::Error::missing_field("kind"))?
        .to_string();
    let id_str = v["id"]
        .as_str()
        .ok_or_else(|| serde::de::Error::missing_field("id"))?
        .to_string();
    Ok(RawEvent { kind_str, id_str })
}

// ── Public subscribe API ─────────────────────────────────────────────────────

/// Subscribe to `ModelEvent<M>` notifications fired by `emit_event`'s
/// publisher hook for `M::table_name`. First call against a pool
/// spawns the listener task; later calls reuse it. Subscribers against
/// the same model + pool share one broadcast channel.
/// # Channel naming
/// `format!("djogi_{}", M::table_name)` — publisher and subscriber
/// derive the same name, no runtime coordination needed.
/// # Errors
/// - `NotifyError::ListenerStartFailed` — listener spawn or `LISTEN`
/// SQL failed.
/// - `NotifyError::ListenerTerminated` — the existing pool listener's
/// watcher task already exited (Postgres terminated our backend,
/// socket dropped, watcher panicked, or senders mutex poisoned).
/// `subscribe` reaps the dead listener and tries to spawn fresh; if
/// the freshly-spawned listener also dies before this call returns
/// (extremely rare race), the caller sees this variant and a retry
/// converges on a healthy listener.
/// - `NotifyError::PayloadDecode` (delivered via `recv.await`)
/// the wire payload's `kind` was not one of `"create" | "save" |
/// "delete"`.
/// - `NotifyError::InvalidId` (also via `recv.await`)
/// `M::Pk::from_str` rejected the wire id string.
/// **Note on parse failures.** A wire payload that does not parse as
/// JSON at all is logged via `tracing::warn!` (target `djogi::notify`)
/// and dropped at the listener boundary — subscribers do not see
/// these as `recv` errors. Only payloads that parse but fail
/// downstream decoding surface as `PayloadDecode` / `InvalidId`.
pub async fn subscribe<M>(pool: &DjogiPool) -> Result<TypedReceiver<M>, NotifyError>
where
    M: crate::model::Model + 'static,
    M::Pk: FromStr + Send + Sync + 'static,
    <M::Pk as FromStr>::Err: std::fmt::Display + Send + Sync + 'static,
{
    let listener = get_or_start_listener(pool).await?;
    let channel = format!("djogi_{}", M::table_name());

    // Validate the channel name BEFORE registering a Sender so a bad
    // ident never leaves an orphan `broadcast::Sender` stranded in the
    // map. `LISTEN` doesn't accept bind parameters, so the name
    // interpolates directly into SQL — defense-in-depth even though
    // `M::table_name` is proc-macro-validated upstream.
    crate::ident::check_plain_ident(&channel, false).map_err(|e| {
        NotifyError::ListenerStartFailed(format!("subscribe: invalid channel {channel:?}: {e:?}"))
    })?;

    // Fast-path: if the watcher already died before we entered, bail
    // out immediately rather than registering a Sender on a dead
    // listener. The post-insert check below handles the race where
    // the watcher dies between this check and the insert.
    if listener.is_failed() {
        return Err(NotifyError::ListenerTerminated);
    }

    let raw_rx = {
        // Tolerate a poisoned lock by recovering the inner data via
        // `into_inner`. A poison means a prior holder panicked, but
        // the map's structural integrity is preserved (we never
        // partially mutate inside a poisonable scope). Returning the
        // `PoisonError` directly would force callers to handle a
        // brand-new error variant for an internal recoverable hazard.
        let mut senders = listener
            .senders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tx = senders
            .entry(channel.clone())
            .or_insert_with(|| broadcast::Sender::new(256));
        tx.subscribe()
    };

    // Late-watcher-death race: the watcher may have exited between
    // the fast-path check above and our Sender insert. The exit
    // guard's clear-and-publish runs concurrently. Re-check `failed`
    // explicitly so we don't return a `TypedReceiver` wedded to a
    // dead Sender pump that nobody will ever drive.
    // Three interleavings are possible:
    // (a) guard ran BEFORE our lock — map was empty; we inserted
    // a fresh Sender that now has no pump → remove + return
    // `ListenerTerminated`.
    // (b) guard ran AFTER our lock — guard cleared the map (and
    // our Sender with it); the broadcast::Receiver we already
    // hold sees the channel close on first `recv`. We still
    // want subscribe to fail synchronously here so the adopter
    // reaches the retry/respawn path immediately.
    // (c) guard ran INTERLEAVED — same outcome as (a) or (b).
    // In all three, `is_failed` is true and we return Err.
    if listener.is_failed() {
        let mut senders = listener
            .senders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Best-effort cleanup of the Sender we may have just inserted.
        // No-op if the guard already cleared the map.
        senders.remove(&channel);
        return Err(NotifyError::ListenerTerminated);
    }

    let listen_sql = format!("LISTEN {channel}");
    listener
        .client
        .simple_query(&listen_sql)
        .await
        .map_err(|e| NotifyError::ListenerStartFailed(format!("LISTEN failed: {e}")))?;

    Ok(TypedReceiver {
        raw: raw_rx,
        _listener: listener,
        _model: PhantomData,
    })
}

/// Typed wrapper around the underlying raw broadcast receiver. Each
/// `recv.await` decodes the next raw event into a `ModelEvent<M>`.
/// Holds an `Arc<PgListener>` keepalive — the listener task stays up
/// for at least as long as this receiver is alive.
pub struct TypedReceiver<M: crate::model::Model> {
    raw: broadcast::Receiver<RawEvent>,
    #[allow(dead_code)] // lifecycle anchor, never read
    _listener: Arc<PgListener>,
    _model: PhantomData<M>,
}

impl<M> TypedReceiver<M>
where
    M: crate::model::Model + 'static,
    M::Pk: FromStr,
    <M::Pk as FromStr>::Err: std::fmt::Display,
{
    /// Await the next event.
    /// # Errors
    /// - [`NotifyError::ChannelLagged`] — the broadcast channel buffer
    /// overflowed since the last `recv` (slow consumer). The integer
    /// payload is the count of skipped events; adopters decide
    /// whether to re-fetch full state or ignore the gap.
    /// - [`NotifyError::ListenerTerminated`] — the watcher task
    /// driving this listener has exited (Postgres terminated our
    /// backend, the underlying socket dropped, the watcher's
    /// senders mutex was poisoned, or the watcher panicked). The
    /// broadcast channel saw its last `Sender` drop when the exit
    /// guard cleared the senders map. Adopters recover by calling
    /// [`subscribe::<M>`] again — it detects the failed listener,
    /// reaps the registry slot, and spawns a fresh listener.
    /// - [`NotifyError::PayloadDecode`] /
    /// [`NotifyError::InvalidId`] — wire payload didn't match the
    /// `{"kind":"...","id":"..."}` schema. Diagnostic, not fatal.
    pub async fn recv(&mut self) -> Result<ModelEvent<M>, NotifyError> {
        let raw = self.raw.recv().await.map_err(|e| match e {
            broadcast::error::RecvError::Lagged(n) => NotifyError::ChannelLagged { skipped: n },
            broadcast::error::RecvError::Closed => NotifyError::ListenerTerminated,
        })?;
        decode_event::<M>(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::model::Model;
    use heeranjid::HeerId;

    /// Minimal Model impl that avoids the proc-macro harness — only
    /// `M::Pk` is exercised by the decode tests.
    struct FakeModel;
    impl crate::model::__sealed::Sealed for FakeModel {}
    #[allow(clippy::manual_async_fn)]
    impl Model for FakeModel {
        type Pk = HeerId;
        type Fields = ();
        fn table_name() -> &'static str {
            "fakes"
        }
        fn pk_value(&self) -> &Self::Pk {
            unreachable!("decode tests don't call pk_value")
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!("decode tests don't call descriptor")
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: Self::Pk,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
    }

    #[test]
    fn decode_create_round_trips() {
        let payload = r#"{"kind":"create","id":"42"}"#;
        let event = decode_payload::<FakeModel>(payload).expect("decode");
        assert_eq!(event.kind, EventKind::Created);
        assert_eq!(event.id, HeerId::from_i64(42).unwrap());
    }

    #[test]
    fn decode_save_maps_to_updated() {
        let payload = r#"{"kind":"save","id":"100"}"#;
        let event = decode_payload::<FakeModel>(payload).expect("decode");
        assert_eq!(event.kind, EventKind::Updated);
        assert_eq!(event.id, HeerId::from_i64(100).unwrap());
    }

    #[test]
    fn decode_delete_round_trips() {
        let payload = r#"{"kind":"delete","id":"7"}"#;
        let event = decode_payload::<FakeModel>(payload).expect("decode");
        assert_eq!(event.kind, EventKind::Deleted);
        assert_eq!(event.id, HeerId::from_i64(7).unwrap());
    }

    #[test]
    fn decode_unknown_kind_errors() {
        let payload = r#"{"kind":"unknown","id":"1"}"#;
        let result = decode_payload::<FakeModel>(payload);
        assert!(matches!(result, Err(NotifyError::PayloadDecode { .. })));
    }

    #[test]
    fn decode_invalid_id_errors() {
        let payload = r#"{"kind":"create","id":"not-a-number"}"#;
        let result = decode_payload::<FakeModel>(payload);
        assert!(matches!(result, Err(NotifyError::InvalidId { .. })));
    }

    #[test]
    fn decode_missing_kind_errors() {
        let payload = r#"{"id":"1"}"#;
        let result = decode_payload::<FakeModel>(payload);
        assert!(matches!(result, Err(NotifyError::PayloadDecode { .. })));
    }

    #[test]
    fn decode_malformed_json_errors() {
        let payload = "not json at all";
        let result = decode_payload::<FakeModel>(payload);
        assert!(matches!(result, Err(NotifyError::PayloadDecode { .. })));
    }

    #[test]
    fn upgrade_existing_returns_strong_when_alive() {
        let map: Mutex<HashMap<u64, Weak<u32>>> = Mutex::new(HashMap::new());
        let live = Arc::new(123u32);
        map.lock().unwrap().insert(1, Arc::downgrade(&live));

        let upgraded = upgrade_existing(&map, 1).expect("upgrade should succeed while live");
        assert_eq!(*upgraded, 123);
        // Live entry stays in the map after a successful upgrade.
        assert!(map.lock().unwrap().contains_key(&1));
    }

    #[test]
    fn upgrade_existing_returns_none_and_reaps_after_drop() {
        let map: Mutex<HashMap<u64, Weak<u32>>> = Mutex::new(HashMap::new());
        {
            let dying = Arc::new(456u32);
            map.lock().unwrap().insert(2, Arc::downgrade(&dying));
            // `dying` drops here, leaving the Weak dangling.
        }
        // Slot is still present (just dangling) until lookup runs.
        assert!(map.lock().unwrap().contains_key(&2));

        let result = upgrade_existing(&map, 2);
        assert!(result.is_none(), "dangling Weak should fail to upgrade");
        assert!(
            !map.lock().unwrap().contains_key(&2),
            "stale entry should be reaped on the failed-upgrade lookup"
        );
    }

    #[test]
    fn upgrade_existing_returns_none_for_missing_key() {
        let map: Mutex<HashMap<u64, Weak<u32>>> = Mutex::new(HashMap::new());
        assert!(upgrade_existing(&map, 99).is_none());
    }

    #[test]
    fn remove_if_current_removes_matching_entry() {
        let map: Mutex<HashMap<u64, Weak<u32>>> = Mutex::new(HashMap::new());
        let failed = Arc::new(123u32);
        map.lock().unwrap().insert(7, Arc::downgrade(&failed));

        assert!(
            remove_if_current(&map, 7, &failed),
            "matching allocation should be removed"
        );
        assert!(
            !map.lock().unwrap().contains_key(&7),
            "registry slot must be empty after matching removal"
        );
    }

    #[test]
    fn remove_if_current_preserves_raced_in_replacement() {
        let map: Mutex<HashMap<u64, Weak<u32>>> = Mutex::new(HashMap::new());
        let failed = Arc::new(11u32);
        let healthy = Arc::new(22u32);
        map.lock().unwrap().insert(5, Arc::downgrade(&failed));

        let observed_failed = upgrade_existing(&map, 5).expect("failed listener still strong");
        assert!(Arc::ptr_eq(&failed, &observed_failed));

        // Simulate another resubscriber winning the recovery race:
        // it has already replaced the registry slot with a healthy
        // listener while this caller still holds `observed_failed`.
        map.lock().unwrap().insert(5, Arc::downgrade(&healthy));

        assert!(
            !remove_if_current(&map, 5, &observed_failed),
            "stale failed-listener cleanup must not erase a healthy replacement"
        );
        let remaining = map
            .lock()
            .unwrap()
            .get(&5)
            .and_then(Weak::upgrade)
            .expect("healthy replacement should remain in registry");
        assert!(
            Arc::ptr_eq(&healthy, &remaining),
            "registry must still point at the raced-in healthy listener"
        );
    }

    #[test]
    fn install_or_lose_first_caller_wins_slot() {
        let map: Mutex<HashMap<u64, Weak<u32>>> = Mutex::new(HashMap::new());
        let candidate = Arc::new(7u32);

        let installed = install_or_lose(&map, 10, Arc::clone(&candidate));
        // First caller's Arc is the canonical one — same allocation.
        assert!(Arc::ptr_eq(&candidate, &installed));
        // Slot now holds a Weak pointing at the same allocation.
        let weak = map.lock().unwrap().get(&10).cloned().unwrap();
        let upgraded = weak.upgrade().expect("Weak should upgrade");
        assert!(Arc::ptr_eq(&candidate, &upgraded));
    }

    #[test]
    fn install_or_lose_second_caller_loses_to_existing() {
        let map: Mutex<HashMap<u64, Weak<u32>>> = Mutex::new(HashMap::new());
        let winner = Arc::new(11u32);
        let loser = Arc::new(22u32);

        // First insertion seeds the slot.
        let _winner_kept = install_or_lose(&map, 5, Arc::clone(&winner));

        // Second caller arrives with a different candidate; it must
        // be discarded in favour of the existing winner.
        let resolved = install_or_lose(&map, 5, Arc::clone(&loser));
        assert!(
            Arc::ptr_eq(&winner, &resolved),
            "racer should resolve to the canonical winner, not its own candidate"
        );
        // Loser still alive locally (it's a separate Arc), but the
        // map's Weak still points at the winner.
        assert_eq!(*loser, 22);
        let weak = map.lock().unwrap().get(&5).cloned().unwrap();
        let upgraded = weak.upgrade().expect("winner Weak should upgrade");
        assert!(Arc::ptr_eq(&winner, &upgraded));
    }

    #[test]
    fn install_or_lose_revives_dangling_slot() {
        let map: Mutex<HashMap<u64, Weak<u32>>> = Mutex::new(HashMap::new());
        // Seed a dangling Weak.
        {
            let transient = Arc::new(33u32);
            map.lock().unwrap().insert(8, Arc::downgrade(&transient));
        }
        assert!(map.lock().unwrap().contains_key(&8));

        // A fresh candidate should overwrite the dangling slot.
        let fresh = Arc::new(44u32);
        let resolved = install_or_lose(&map, 8, Arc::clone(&fresh));
        assert!(
            Arc::ptr_eq(&fresh, &resolved),
            "fresh candidate should replace the dangling Weak"
        );
        let weak = map.lock().unwrap().get(&8).cloned().unwrap();
        let upgraded = weak.upgrade().expect("fresh Weak should upgrade");
        assert!(Arc::ptr_eq(&fresh, &upgraded));
    }

    #[test]
    fn pool_id_is_unique_per_build() {
        // Two consecutive builder.build calls allocate distinct ids.
        let a = crate::pg::pool::next_pool_id();
        let b = crate::pg::pool::next_pool_id();
        assert_ne!(
            a, b,
            "next_pool_id must allocate distinct ids on consecutive calls"
        );
    }

    // ── GH#131 watcher-died-but-listener-alive lifecycle gap ────────────────
    // The following tests pin the bug-fix behavior. Each one fails on
    // the pre-fix code (no `WatcherExitGuard`, no `failed` flag, raw
    // `expect("notify senders mutex poisoned")` in the watcher hot
    // path). They are pure-Rust unit tests — no Postgres, no async
    // runtime quirks — so they run in CI and `cargo test` alike
    // without any flake surface.

    /// Helper: install a `broadcast::Sender` for `channel` in `senders`
    /// and return a `Receiver` cloned off it. Mirrors the subscribe-side
    /// shape so the assertions below resemble real usage.
    fn install_sender_and_subscribe(
        senders: &Mutex<HashMap<String, broadcast::Sender<RawEvent>>>,
        channel: &str,
        capacity: usize,
    ) -> broadcast::Receiver<RawEvent> {
        let mut guard = senders.lock().unwrap();
        let tx = guard
            .entry(channel.to_string())
            .or_insert_with(|| broadcast::Sender::new(capacity));
        tx.subscribe()
    }

    #[test]
    fn close_all_senders_drops_each_sender_and_wakes_receivers() {
        let senders: Mutex<HashMap<String, broadcast::Sender<RawEvent>>> =
            Mutex::new(HashMap::new());
        let mut rx_a = install_sender_and_subscribe(&senders, "djogi_a", 8);
        let mut rx_b = install_sender_and_subscribe(&senders, "djogi_b", 8);

        // Sanity: both receivers are open (Empty, not Closed) before clear.
        assert!(matches!(
            rx_a.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            rx_b.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));

        close_all_senders(&senders);

        assert!(
            senders.lock().unwrap().is_empty(),
            "close_all_senders must clear the map"
        );
        // Each Sender drop triggers Closed on its Receiver — this is
        // the bug-fix behavior: pre-fix, receivers stayed Empty
        // forever because the Sender was kept alive in the map.
        assert!(
            matches!(rx_a.try_recv(), Err(broadcast::error::TryRecvError::Closed)),
            "receiver A must see Closed once its Sender is dropped from the map"
        );
        assert!(
            matches!(rx_b.try_recv(), Err(broadcast::error::TryRecvError::Closed)),
            "receiver B must see Closed once its Sender is dropped from the map"
        );
    }

    #[test]
    fn close_all_senders_recovers_from_poisoned_lock() {
        let senders: Arc<Mutex<HashMap<String, broadcast::Sender<RawEvent>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx = install_sender_and_subscribe(&senders, "djogi_a", 4);

        // Poison the mutex by panicking while holding it.
        let poison_target = Arc::clone(&senders);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poison_target.lock().unwrap();
            panic!("inducing poison");
        }));
        assert!(
            senders.is_poisoned(),
            "lock should be poisoned by the panic-while-locked path"
        );

        // Helper still clears despite poison.
        close_all_senders(&senders);

        let cleared = senders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            cleared.is_empty(),
            "close_all_senders must clear despite a poisoned lock — \
             without recover-via-into_inner the watcher exit guard \
             would re-panic instead of cleaning up"
        );
        // Receiver still surfaces Closed because its Sender was dropped.
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Closed)
        ));
    }

    #[test]
    fn watcher_exit_guard_drop_clears_senders_and_marks_failed() {
        let senders: Arc<Mutex<HashMap<String, broadcast::Sender<RawEvent>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let failed = Arc::new(AtomicBool::new(false));

        // Simulate a live subscription before the watcher dies — the
        // exact shape `subscribe::<M>` produces.
        let mut rx = install_sender_and_subscribe(&senders, "djogi_phase8_t11_evt", 16);

        // Sanity: receiver is empty-but-open; failed flag is false.
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert!(!failed.load(Ordering::Acquire));

        let guard = WatcherExitGuard {
            senders: Arc::clone(&senders),
            failed: Arc::clone(&failed),
        };

        drop(guard);

        assert!(
            failed.load(Ordering::Acquire),
            "WatcherExitGuard::drop must publish failed=true so \
             subscribers reap the dead listener"
        );
        assert!(
            senders.lock().unwrap().is_empty(),
            "WatcherExitGuard::drop must clear the senders map"
        );
        // This is the bug fix for GH#131: pre-fix, this assertion
        // failed (the receiver stayed Empty forever because the
        // Sender survived in the map after the watcher exited).
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Closed)),
            "live receivers must see Closed after the watcher exit guard \
             fires — pre-fix they hung forever (GH#131)"
        );
    }

    #[test]
    fn watcher_exit_guard_fires_on_panic_unwind() {
        let senders: Arc<Mutex<HashMap<String, broadcast::Sender<RawEvent>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let failed = Arc::new(AtomicBool::new(false));
        let mut rx = install_sender_and_subscribe(&senders, "djogi_a", 4);

        let senders_for_panic = Arc::clone(&senders);
        let failed_for_panic = Arc::clone(&failed);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = WatcherExitGuard {
                senders: senders_for_panic,
                failed: failed_for_panic,
            };
            panic!("watcher task panicked mid-loop");
        }));

        assert!(
            result.is_err(),
            "panic should propagate to catch_unwind; if it doesn't, \
             the test isn't actually exercising the panic-unwind path"
        );
        // Drop on panic unwind must still fire — this is what makes
        // the guard pattern robust against GH#131 path 1 (panic in
        // senders lookup).
        assert!(
            failed.load(Ordering::Acquire),
            "failed flag must be published on panic unwind"
        );
        assert!(
            senders.lock().unwrap_or_else(|p| p.into_inner()).is_empty(),
            "senders map must be cleared on panic unwind"
        );
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Closed)),
            "live receivers must see Closed even when the watcher \
             panicked rather than exiting cleanly"
        );
    }

    #[test]
    fn watcher_exit_guard_with_no_subscribers_is_a_safe_no_op() {
        // Clean-shutdown case: PgListener drops because the last
        // subscriber dropped. No live receivers; the guard still
        // fires but nothing observable. This pins that the guard
        // doesn't panic or misbehave on an empty senders map.
        let senders: Arc<Mutex<HashMap<String, broadcast::Sender<RawEvent>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let failed = Arc::new(AtomicBool::new(false));

        let guard = WatcherExitGuard {
            senders: Arc::clone(&senders),
            failed: Arc::clone(&failed),
        };
        drop(guard);

        assert!(failed.load(Ordering::Acquire));
        assert!(senders.lock().unwrap().is_empty());
    }

    #[test]
    fn pg_listener_is_failed_reflects_failed_flag() {
        // Synthetic setup: build the failed flag the same way
        // spawn_listener does and observe through the shared Arc.
        // We can't construct a real PgListener without a live
        // tokio_postgres::Client, but is_failed only reads the flag.
        let failed = Arc::new(AtomicBool::new(false));

        // Mirrors PgListener::is_failed — Acquire load.
        let read_is_failed = || failed.load(Ordering::Acquire);

        assert!(!read_is_failed(), "fresh listener starts unfailed");
        failed.store(true, Ordering::Release);
        assert!(
            read_is_failed(),
            "Acquire load must observe the Release store published by the exit guard"
        );
    }
}
