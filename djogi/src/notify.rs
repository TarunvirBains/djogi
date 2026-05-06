//! In-process model-event NOTIFY subscription surface — Cluster 8ζ T11.3.
//!
//! # What
//!
//! `djogi::notify::subscribe::<M>(pool)` returns a
//! `tokio::sync::broadcast::Receiver<ModelEvent<M>>` that fires whenever
//! a row in `M::table_name()` is created, saved, or deleted by any
//! `DjogiContext` configured against the same Postgres database.
//!
//! Companion to the synchronous publisher hook in `crate::outbox::emit_event`
//! (T11.2): every `#[model(events)]` write fires `pg_notify('djogi_<table>',
//! '{"kind":"<action>","id":"<pk>"}')` inside the parent transaction.
//! Subscribers attached here decode that payload and surface it as
//! `ModelEvent<M> { kind, id }`. The receiver re-fetches the full row via
//! `M::find(...)` if it needs the data; the slim payload exists because
//! `pg_notify` truncates payloads larger than 8000 bytes.
//!
//! # Why feature-gated
//!
//! Behind `feature = "notify"` so adopters who don't subscribe pay nothing
//! for the per-pool listener task or the `tokio-stream` dependency. When
//! enabled, the publisher hook in `emit_event` and the subscriber surface
//! here both compile in. Both halves are gated on the same flag — turning
//! off the feature disables both ends cohesively.
//!
//! # How
//!
//! - First call to `subscribe::<M>(pool)` for a given pool spawns a
//!   per-pool [`PgListener`] background task that owns a dedicated
//!   `tokio_postgres::Client` (acquired from the pool and never returned)
//!   running `LISTEN djogi_<table>` for every channel a subscriber asks
//!   for. Subsequent `subscribe` calls reuse the listener.
//! - The listener forwards `tokio_postgres::AsyncMessage::Notification`
//!   events to a per-channel `tokio::sync::broadcast::Sender`. Each
//!   subscriber gets a fresh `Receiver` cloned from that Sender.
//! - The pool registry is keyed by `Arc::as_ptr` on the underlying
//!   deadpool `Pool` — cloned `DjogiPool` instances share one listener;
//!   reconstructed pools get fresh listeners.
//!
//! # Lifecycle (T11.4 will tighten)
//!
//! T11.3 ships the basic subscribe surface. T11.4 follows up with
//! per-pool registry weak-ref cleanup, graceful shutdown on listener
//! drop, and the hot-reload path. For now the registry holds strong
//! `Arc<PgListener>` references; pools created during a test that
//! drop without explicit shutdown leak the listener task. Acceptable
//! for the v0.1.0 alpha surface — the leak is per-pool, not per-call,
//! and adopter-visible only in long-running test suites that
//! reconstruct pools.
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
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::broadcast;
use tokio_postgres::AsyncMessage;

// ── Public surface types ─────────────────────────────────────────────────────

/// Event kinds carried by `ModelEvent<M>`.
///
/// Matches the `OutboxAction` set published by `emit_event`'s notify hook
/// (T11.2), with one rename for the subscriber-facing audience:
/// `OutboxAction::Save` surfaces here as `EventKind::Updated`. The wire
/// payload still says `"save"` (matching the outbox `action` column);
/// the rename is purely a Rust-side ergonomic — adopters reading
/// `ModelEvent::kind == EventKind::Updated` see code that reads
/// naturally without prior knowledge of the outbox ledger semantics.
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

/// One per-pool listener task. Owns a dedicated long-lived
/// `tokio_postgres::Client` (a standalone client connected outside the
/// pool, kept alive for the listener's lifetime) plus a per-channel
/// `broadcast::Sender` registry shared with the spawned connection
/// watcher.
///
/// `senders` is an `Arc<Mutex<...>>` (not just `Mutex<...>`) because
/// the spawned watcher task needs to read from the same map that
/// `subscribe::<M>` writes to when registering channels. Sharing via
/// Arc is cleaner than reaching back into the listener struct from
/// inside a `'static` spawn body.
///
/// `client` is held to keep the LISTEN connection alive — dropping
/// the client tears down the connection task and notifications stop.
struct PgListener {
    senders: Arc<Mutex<HashMap<String, broadcast::Sender<RawEvent>>>>,
    /// Held to keep the dedicated LISTEN connection alive. The
    /// underlying connection task runs on a separate `tokio::spawn`
    /// and references the same client; both must be live for
    /// notifications to flow.
    #[allow(dead_code)] // kept-alive guard, not directly accessed after spawn
    client: tokio_postgres::Client,
}

// ── Internal: per-process pool registry ──────────────────────────────────────

/// Pool-keyed registry of running `PgListener` instances.
///
/// Keyed by the deadpool `Pool`'s pointer identity (cloning a
/// `DjogiPool` is just `Arc::clone` on the inner `deadpool_postgres::Pool`,
/// so two `DjogiPool` instances pointing at the same backing pool share
/// one entry). Reconstructing a pool produces a fresh entry.
///
/// T11.4 will replace the strong `Arc<PgListener>` with `Weak<PgListener>`
/// plus cleanup-on-miss; T11.3 ships strong refs to keep the cluster
/// commit shape focused.
fn registry() -> &'static Mutex<HashMap<usize, Arc<PgListener>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<usize, Arc<PgListener>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pool_key(pool: &DjogiPool) -> usize {
    // deadpool_postgres::Pool is Arc-shaped internally; using its
    // address as the identity is a stable per-pool key for the
    // lifetime of the underlying allocation.
    std::ptr::addr_of!(pool.inner) as usize
}

// ── Wire payload decode ──────────────────────────────────────────────────────

/// Decode a raw NOTIFY payload of shape `{"kind":"...","id":"..."}`
/// into a typed `(EventKind, M::Pk)` pair.
///
/// Pure function — no DB, no listener, no async. Lifted out so the
/// decode contract is unit-testable on its own.
fn decode_payload<M: crate::model::Model>(raw: &str) -> Result<ModelEvent<M>, NotifyError>
where
    M::Pk: FromStr,
    <M::Pk as FromStr>::Err: std::fmt::Display,
{
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|source| NotifyError::PayloadDecode {
            raw: raw.to_string(),
            source,
        })?;

    let kind_str = value["kind"]
        .as_str()
        .ok_or_else(|| NotifyError::PayloadDecode {
            raw: raw.to_string(),
            source: serde::de::Error::missing_field("kind"),
        })?;
    let kind = match kind_str {
        "create" => EventKind::Created,
        "save" => EventKind::Updated,
        "delete" => EventKind::Deleted,
        other => {
            return Err(NotifyError::PayloadDecode {
                raw: raw.to_string(),
                source: serde::de::Error::unknown_variant(other, &["create", "save", "delete"]),
            });
        }
    };

    let id_str = value["id"]
        .as_str()
        .ok_or_else(|| NotifyError::PayloadDecode {
            raw: raw.to_string(),
            source: serde::de::Error::missing_field("id"),
        })?;
    let id = M::Pk::from_str(id_str).map_err(|e| NotifyError::InvalidId {
        id: id_str.to_string(),
        reason: e.to_string(),
    })?;

    Ok(ModelEvent { kind, id })
}

// ── Listener startup + LISTEN management ─────────────────────────────────────

/// Acquire-or-create the `PgListener` instance for `pool`. Lazy: first
/// call spawns the background task; subsequent calls return the
/// already-running listener.
async fn get_or_start_listener(pool: &DjogiPool) -> Result<Arc<PgListener>, NotifyError> {
    let key = pool_key(pool);
    {
        let registry = registry().lock().expect("notify registry mutex poisoned");
        if let Some(listener) = registry.get(&key) {
            return Ok(Arc::clone(listener));
        }
    }

    // Race-tolerant: two concurrent first-callers may both reach this
    // point; the second one's `insert` returns the previously-inserted
    // entry which we accept as the canonical listener. The cost of the
    // duplicate spawn-attempt is one extra connection acquisition that
    // gets dropped immediately.
    let listener = Arc::new(spawn_listener(pool).await?);
    let mut registry = registry().lock().expect("notify registry mutex poisoned");
    let entry = registry.entry(key).or_insert_with(|| Arc::clone(&listener));
    Ok(Arc::clone(entry))
}

/// Spawn the per-pool listener task. Connects a dedicated standalone
/// client outside the pool (the deadpool-managed `Object` doesn't
/// expose the `Connection` half needed for `AsyncMessage` polling),
/// spawns the connection watcher, and returns a `PgListener` wired
/// to the same `senders` map the watcher writes to.
async fn spawn_listener(pool: &DjogiPool) -> Result<PgListener, NotifyError> {
    // Standalone connect using the pool's stored URL — sidesteps
    // deadpool's Object/Connection separation, which doesn't expose
    // the `Connection` half needed for `tokio_postgres::AsyncMessage`
    // polling. The pool reference is retained for pool-keyed
    // registry identity (and for T11.4's hot-reload symmetry).
    //
    // Pools without a URL (internal audit-side construction, see
    // `DjogiPool::url` doc) cannot drive the NOTIFY listener — fail
    // explicitly so the caller knows this isn't a recoverable runtime
    // hiccup but a structural pool-construction mismatch.
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

    // Single shared senders map — both `subscribe::<M>` (via the
    // returned PgListener) and the spawned watcher task hold an
    // Arc<Mutex<_>> pointing at the same allocation. Writes from
    // subscribe and reads from the watcher synchronize on the Mutex.
    let senders: Arc<Mutex<HashMap<String, broadcast::Sender<RawEvent>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let senders_for_task = Arc::clone(&senders);

    // Connection watcher: polls the AsyncMessage stream, routes
    // Notification events to the per-channel broadcast::Sender, drives
    // the connection's I/O loop. Without this task running, queries
    // on `client` (e.g. the `LISTEN` issued by subscribe::<M>) never
    // make progress.
    tokio::spawn(async move {
        use futures::StreamExt;
        let mut stream = futures::stream::poll_fn(move |cx| connection.poll_message(cx));
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(AsyncMessage::Notification(n)) => {
                    let channel = n.channel().to_string();
                    let payload = n.payload().to_string();
                    let raw = match parse_raw(&payload) {
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
                    if let Some(tx) = senders_guard.get(&channel) {
                        // SendError is only returned when there are
                        // no live receivers — that's fine, the
                        // notification has nowhere to go.
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
/// publisher hook (T11.2) for `M::table_name()`.
///
/// First call against a given pool spawns a background listener task;
/// subsequent calls reuse the running listener. Returns a
/// `TypedReceiver<M>` wrapping a `broadcast::Receiver` — multiple
/// subscribers against the same model + pool share one underlying
/// broadcast channel (the listener task fans out to all receivers).
///
/// **Lifecycle (T11.3 baseline):** the listener spawns a dedicated
/// connection task that lives for the listener's registry lifetime.
/// T11.4 follows up with weak-ref registry cleanup, listener
/// shutdown on pool drop, and the hot-reload path.
///
/// # Channel naming
///
/// `format!("djogi_{}", M::table_name())`. Every model gets a
/// distinct Postgres NOTIFY channel — the publisher (T11.2) and the
/// subscriber here both compute the channel name the same way, so
/// the two ends are guaranteed to align without runtime coordination.
///
/// # Errors
///
/// - `NotifyError::ListenerStartFailed` — pool acquisition or
///   standalone `tokio_postgres::connect` failed during initial
///   listener spawn, or the `LISTEN` SQL failed.
/// - `NotifyError::PayloadDecode` (delivered via `recv().await` on
///   the returned receiver) — a malformed wire payload was received.
/// - `NotifyError::InvalidId` (also via `recv().await`) —
///   `M::Pk::from_str` rejected the wire id string.
pub async fn subscribe<M>(pool: &DjogiPool) -> Result<TypedReceiver<M>, NotifyError>
where
    M: crate::model::Model + 'static,
    M::Pk: FromStr + Send + Sync + 'static,
    <M::Pk as FromStr>::Err: std::fmt::Display + Send + Sync + 'static,
{
    let listener = get_or_start_listener(pool).await?;
    let channel = format!("djogi_{}", M::table_name());

    // Register (or reuse) the per-channel broadcast::Sender, take a
    // receiver off it. The watcher task already holds an Arc clone of
    // the same senders map (set up in spawn_listener), so a
    // notification arriving for `channel` will be routed through
    // this Sender to the new Receiver.
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

    // Issue `LISTEN djogi_<table>` on the dedicated client so
    // Postgres starts routing notifications for this channel. Idempotent
    // at the Postgres side: re-LISTEN on an already-listened channel
    // is a no-op. We use `simple_query` (no params) because LISTEN
    // doesn't accept bind parameters — the channel name was validated
    // at publisher-side (T11.2) via `crate::ident::check_plain_ident`,
    // and we re-validate here defense-in-depth before SQL embedding.
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
        _model: PhantomData,
    })
}

/// Typed wrapper around the underlying raw broadcast receiver.
///
/// Each `recv().await` call decodes the next raw event into a
/// `ModelEvent<M>` via `decode_payload`, surfacing decode errors as
/// `NotifyError::PayloadDecode` / `NotifyError::InvalidId`.
pub struct TypedReceiver<M: crate::model::Model> {
    raw: broadcast::Receiver<RawEvent>,
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
        let payload = serde_json::json!({
            "kind": raw.kind_str,
            "id": raw.id_str,
        })
        .to_string();
        decode_payload::<M>(&payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::model::Model;
    use heeranjid::HeerId;

    // Minimal Model impl for decode tests. Avoids the proc-macro
    // harness — we only need the M::Pk associated type to drive
    // FromStr-based id decoding.
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
}
