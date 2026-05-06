//! In-process model-event NOTIFY subscription surface — Cluster 8ζ T11.
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
//!   `tokio_postgres::Client` (a standalone connection outside the
//!   pool, so `tokio_postgres::AsyncMessage` polling is available)
//!   running `LISTEN djogi_<table>` for every channel a subscriber
//!   asks for. Subsequent `subscribe` calls reuse the listener.
//! - The listener forwards `tokio_postgres::AsyncMessage::Notification`
//!   events to a per-channel `tokio::sync::broadcast::Sender`. Each
//!   subscriber gets a fresh `Receiver` cloned from that Sender.
//! - The pool registry is keyed by [`DjogiPool::pool_id`], a
//!   per-process unique id allocated at pool construction and copied
//!   verbatim on `Clone`. Cloned `DjogiPool` instances therefore share
//!   one listener; freshly-built pools get fresh listeners. The slot
//!   holds a `Weak<PgListener>` so the registry never prolongs the
//!   listener's life — see the "Lifecycle" section.
//!
//! # Lifecycle
//!
//! Three drop / restart paths are handled cohesively:
//!
//! 1. **Subscriber drop.** `TypedReceiver<M>` is just a thin wrapper
//!    around `tokio::sync::broadcast::Receiver<RawEvent>`; dropping
//!    it returns the receiver slot to the broadcast channel. The
//!    listener task and the per-channel `Sender` live on so other
//!    subscribers (or future ones) keep receiving.
//! 2. **Pool drop.** The registry stores `Weak<PgListener>` keyed by
//!    `DjogiPool::pool_id`. Each `TypedReceiver<M>` returned by
//!    `subscribe` holds an `Arc<PgListener>` keepalive — so the
//!    listener stays up exactly as long as at least one subscriber
//!    is still listening. When the last subscriber drops, the
//!    listener's strong count hits zero and `Drop` on `PgListener`
//!    fires: dropping the dedicated `tokio_postgres::Client` closes
//!    its channel to the spawned connection-watcher task, which
//!    observes the `poll_message` stream end and exits cleanly. The
//!    registry's `Weak` entry becomes dangling and is reclaimed
//!    lazily on the next `subscribe` call against the same `pool_id`
//!    (see `upgrade_existing`).
//! 3. **Hot reload.** A freshly-built `DjogiPool` gets a fresh
//!    `pool_id` (allocated by `crate::pg::pool::next_pool_id`), so
//!    `subscribe` against the new pool always misses the registry
//!    and spawns a new listener — independent of any stale entry the
//!    old pool may have left behind. If the same pool is re-used
//!    (clone) after the last subscriber drops, the next `subscribe`
//!    finds the dangling `Weak`, removes it, and spawns fresh: the
//!    "stop listening, then resume later" path is automatic.
//!
//! The strong-reference contract is therefore: subscribers hold the
//! listener alive; the registry watches without prolonging life.
//! This is the inverse of T11.3's interim arrangement (registry held
//! strong refs, leaking listener tasks across reconstructed pools);
//! T11.4 puts the lifecycle on the subscribers, where it belongs.
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

impl Drop for PgListener {
    fn drop(&mut self) {
        // Dropping `client` (the `tokio_postgres::Client` half of the
        // dedicated standalone connection) closes its request channel.
        // The spawned connection-watcher task then observes
        // `poll_message` returning `Ready(None)` (or an error if the
        // connection was already torn down) and exits its loop on the
        // next poll, releasing its `Arc<Mutex<senders>>` clone.
        //
        // This Drop impl exists for observability — the actual
        // teardown is driven by `client`'s own `Drop`. We log at
        // debug because in production this is benign housekeeping;
        // in tests it's a useful signal that the lifecycle path is
        // exercising correctly.
        tracing::debug!(
            target: "djogi::notify",
            "PgListener dropped — connection watcher will exit, broadcast::Receivers will see Closed"
        );
    }
}

// ── Internal: per-process pool registry ──────────────────────────────────────

/// Pool-keyed registry of running `PgListener` instances.
///
/// Keyed by [`DjogiPool::pool_id`], a per-process unique id allocated
/// at pool construction and copied verbatim on `Clone`. So two
/// `DjogiPool` clones share an entry; freshly-built pools get a
/// fresh entry.
///
/// Stores `Weak<PgListener>` rather than strong refs so the registry
/// observes listeners without prolonging their life. The strong
/// refs live on the [`TypedReceiver<M>`] handles returned by
/// `subscribe`: when the last subscriber drops, the listener winds
/// down and its `Weak` here becomes dangling. Lookups upgrade the
/// `Weak`; misses (because the listener is gone) trigger cleanup of
/// the dangling entry and a fresh spawn.
fn registry() -> &'static Mutex<HashMap<u64, Weak<PgListener>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, Weak<PgListener>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pool_key(pool: &DjogiPool) -> u64 {
    pool.pool_id
}

/// Try to reuse an existing live listener for `key`. If the entry
/// exists and the `Weak` upgrades, return the strong `Arc`. If the
/// entry exists but the `Weak` is dangling (listener already
/// dropped), remove the dangling entry as a side-effect so the next
/// caller doesn't keep paying for a stale lookup.
///
/// Pure registry operation — no async, no DB, no listener spawn.
/// The actual fallback (spawning a fresh listener) belongs to the
/// caller; lifting the registry lookup out makes the lifecycle
/// state-machine unit-testable.
fn upgrade_existing<T>(map: &Mutex<HashMap<u64, Weak<T>>>, key: u64) -> Option<Arc<T>> {
    let mut guard = map.lock().expect("notify registry mutex poisoned");
    if let Some(weak) = guard.get(&key) {
        if let Some(strong) = weak.upgrade() {
            return Some(strong);
        }
        // Stale entry — listener was dropped after the last subscriber
        // released its keepalive. Remove the dangling slot so we don't
        // accumulate dead weak-refs across the lifetime of the process.
        guard.remove(&key);
    }
    None
}

/// Insert a `Weak<T>` into the registry under `key`. If the slot is
/// already occupied (e.g., a concurrent first-caller raced and won),
/// the existing entry wins — we treat the prior insertion as
/// canonical and let our freshly-spawned listener drop on its own.
///
/// Returns the `Arc<T>` that callers should hand to the subscriber
/// — the canonical one (theirs if they won the race; the racer's
/// strong ref upgraded back from the existing `Weak` if they lost).
/// `None` is impossible from the happy path; it can only surface if
/// the prior entry had already been dropped between the racer's
/// insert and our lookup, which is benign and we recurse.
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
/// call spawns the background task; subsequent calls upgrade the
/// existing `Weak` in the registry.
///
/// Hot-reload is automatic: if the registry holds a dangling `Weak`
/// (listener was torn down after the last subscriber dropped), the
/// upgrade fails, the dangling entry is reaped, and a fresh listener
/// is spawned.
async fn get_or_start_listener(pool: &DjogiPool) -> Result<Arc<PgListener>, NotifyError> {
    let key = pool_key(pool);
    if let Some(listener) = upgrade_existing(registry(), key) {
        return Ok(listener);
    }

    // Race-tolerant: two concurrent first-callers may both spawn
    // listeners. `install_or_lose` keeps whichever entry won the
    // registry insertion race; the loser's listener drops out of
    // scope here, its `Drop` impl winds down its connection task,
    // and only the canonical listener stays up. Cost of the lost
    // race: one transient `tokio_postgres::connect` round-trip.
    let candidate = Arc::new(spawn_listener(pool).await?);
    Ok(install_or_lose(registry(), key, candidate))
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
/// **Lifecycle:** every returned `TypedReceiver<M>` holds an
/// `Arc<PgListener>` keepalive. The listener stays up while at
/// least one subscriber for any model on this pool is alive; when
/// the last subscriber drops, the listener's spawned connection
/// task exits cleanly and a future `subscribe` call against a
/// reusable pool spawns a fresh listener. See the module-level
/// "Lifecycle" section for the full state machine.
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
        _listener: listener,
        _model: PhantomData,
    })
}

/// Typed wrapper around the underlying raw broadcast receiver.
///
/// Each `recv().await` call decodes the next raw event into a
/// `ModelEvent<M>` via `decode_payload`, surfacing decode errors as
/// `NotifyError::PayloadDecode` / `NotifyError::InvalidId`.
///
/// The `_listener` field is the lifecycle anchor: it's an
/// `Arc<PgListener>` keepalive so the listener task keeps running
/// for at least as long as this receiver is alive. When the last
/// `TypedReceiver` for a pool drops, the listener's strong count
/// hits zero, `PgListener::drop` fires, and the spawned watcher
/// task exits cleanly. The registry's `Weak` becomes dangling and
/// is reaped on the next `subscribe` against the same `pool_id`.
pub struct TypedReceiver<M: crate::model::Model> {
    raw: broadcast::Receiver<RawEvent>,
    /// Keepalive — see `TypedReceiver` doc.
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

    // ── T11.4 — registry lifecycle helpers ─────────────────────────────────
    //
    // These tests pin the `Weak<T>` upgrade / cleanup state machine in
    // isolation, using `Arc<u32>` as a stand-in for `Arc<PgListener>`.
    // The real listener lifecycle (LISTEN/NOTIFY round-trip across a
    // pool drop, hot-reload via `subscribe` after teardown) requires a
    // reachable Postgres and lives in the integration test added by
    // T11.5.

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
