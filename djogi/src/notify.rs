//! In-process model-event NOTIFY subscription surface.
//!
//! `subscribe::<M>(pool)` returns a `TypedReceiver<M>` that fires whenever
//! a row in `M::table_name()` is created, saved, or deleted by any
//! `DjogiContext` configured against the same Postgres database.
//!
//! Behind `feature = "notify"`. Companion to the publisher hook in
//! `crate::outbox::emit_event`: every `#[model(events)]` write fires
//! `pg_notify('djogi_<table>', '{"kind":"<action>","id":"<pk>"}')` inside
//! the parent transaction. Subscribers decode that payload and surface
//! `ModelEvent<M> { kind, id }`. Adopters re-fetch the full row via
//! `M::find(...)` when they need the columns; the slim id-only payload
//! sidesteps the 8000-byte `pg_notify` cap.
//!
//! # Lifecycle
//!
//! The strong-reference contract: subscribers hold the listener alive,
//! the registry watches without prolonging life. Three drop paths fall
//! out of that:
//!
//! 1. **Subscriber drop.** The receiver slot is returned to the broadcast
//!    channel. The listener and per-channel `Sender` stay up for other
//!    subscribers.
//! 2. **Last-subscriber drop.** The listener's strong count hits zero,
//!    its dedicated `tokio_postgres::Client` drops, the spawned
//!    connection-watcher observes `poll_message` ending and exits. The
//!    registry's `Weak` entry becomes dangling and is reaped lazily on
//!    the next `subscribe` against the same `pool_id`.
//! 3. **Hot reload.** A freshly-built `DjogiPool` gets a fresh `pool_id`,
//!    so `subscribe` against the new pool always misses the registry
//!    and spawns a fresh listener.
//!
//! # Wire schema
//!
//! Stable JSON shape on `djogi_<M::TABLE>` channels:
//!
//! ```json
//! { "kind": "create" | "save" | "delete", "id": "<M::Pk Display>" }
//! ```
//!
//! `kind` strings exactly match `OutboxAction::as_sql_str()` — any
//! schema bump goes through that const for forward-compat.

use crate::pg::pool::DjogiPool;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::broadcast;
use tokio_postgres::AsyncMessage;

// ── Public surface types ─────────────────────────────────────────────────────

/// Event kinds carried by `ModelEvent<M>`.
///
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
///
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
    /// Keepalive for the LISTEN connection — dropping `client` tears
    /// down the connection task and notifications stop.
    #[allow(dead_code)] // kept-alive guard, not directly accessed after spawn
    client: tokio_postgres::Client,
}

impl Drop for PgListener {
    fn drop(&mut self) {
        tracing::debug!(
            target: "djogi::notify",
            "PgListener dropped — watcher will exit, receivers will see Closed"
        );
    }
}

// ── Internal: per-process pool registry ──────────────────────────────────────

/// Pool-keyed registry of running `PgListener` instances.
///
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
/// (`decode_payload`) and the hot-path `recv()` so receivers don't
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
/// the background task, later calls upgrade the existing `Weak`. A
/// dangling `Weak` (listener torn down after the last subscriber
/// dropped) is reaped on lookup and a fresh listener is spawned.
async fn get_or_start_listener(pool: &DjogiPool) -> Result<Arc<PgListener>, NotifyError> {
    let key = pool_key(pool);
    if let Some(listener) = upgrade_existing(registry(), key) {
        return Ok(listener);
    }
    let candidate = Arc::new(spawn_listener(pool).await?);
    Ok(install_or_lose(registry(), key, candidate))
}

/// Spawn the per-pool listener task. Uses a standalone
/// `tokio_postgres::connect` rather than the pool because deadpool's
/// `Object` doesn't expose the `Connection` half needed for
/// `AsyncMessage` polling.
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
    let senders_for_task = Arc::clone(&senders);

    tokio::spawn(async move {
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
                    let senders_guard = senders_for_task
                        .lock()
                        .expect("notify senders mutex poisoned");
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
                        "notify connection terminated; subscribers will stop receiving"
                    );
                    break;
                }
            }
        }
    });

    Ok(PgListener { senders, client })
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
/// publisher hook for `M::table_name()`. First call against a pool
/// spawns the listener task; later calls reuse it. Subscribers against
/// the same model + pool share one broadcast channel.
///
/// # Channel naming
///
/// `format!("djogi_{}", M::table_name())` — publisher and subscriber
/// derive the same name, no runtime coordination needed.
///
/// # Errors
///
/// - `NotifyError::ListenerStartFailed` — listener spawn or `LISTEN`
///   SQL failed.
/// - `NotifyError::PayloadDecode` (delivered via `recv().await`) —
///   the wire payload's `kind` was not one of `"create" | "save" |
///   "delete"`.
/// - `NotifyError::InvalidId` (also via `recv().await`) —
///   `M::Pk::from_str` rejected the wire id string.
///
/// **Note on parse failures.** A wire payload that does not parse as
/// JSON at all is logged via `tracing::warn!` (target `djogi::notify`)
/// and dropped at the listener boundary — subscribers do not see
/// these as `recv()` errors. Only payloads that parse but fail
/// downstream decoding surface as `PayloadDecode` / `InvalidId`.
pub async fn subscribe<M>(pool: &DjogiPool) -> Result<TypedReceiver<M>, NotifyError>
where
    M: crate::model::Model + 'static,
    M::Pk: FromStr + Send + Sync + 'static,
    <M::Pk as FromStr>::Err: std::fmt::Display + Send + Sync + 'static,
{
    let listener = get_or_start_listener(pool).await?;
    let channel = format!("djogi_{}", M::table_name());

    let raw_rx = {
        let mut senders = listener
            .senders
            .lock()
            .expect("notify senders mutex poisoned");
        let tx = senders
            .entry(channel.clone())
            .or_insert_with(|| broadcast::Sender::new(256));
        tx.subscribe()
    };

    // Re-validate the channel name before SQL embedding — LISTEN doesn't
    // accept bind parameters, so the channel name interpolates directly.
    crate::ident::check_plain_ident(&channel, false).map_err(|e| {
        NotifyError::ListenerStartFailed(format!("subscribe: invalid channel {channel:?}: {e:?}"))
    })?;
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
/// `recv().await` decodes the next raw event into a `ModelEvent<M>`.
///
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
    /// Await the next event. Returns `Err(NotifyError::ChannelLagged)`
    /// if the broadcast channel buffer overflowed since the last
    /// receive — adopter decides re-fetch policy.
    pub async fn recv(&mut self) -> Result<ModelEvent<M>, NotifyError> {
        let raw = self.raw.recv().await.map_err(|e| match e {
            broadcast::error::RecvError::Lagged(n) => NotifyError::ChannelLagged { skipped: n },
            broadcast::error::RecvError::Closed => NotifyError::ListenerStartFailed(
                "broadcast channel closed (listener task terminated)".to_string(),
            ),
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
}
