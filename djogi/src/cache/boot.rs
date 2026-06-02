//! Boot-time `Sassi` registration via the `inventory` crate.
//! `#[model]` (via `model::cacheable::expand` — when `Cacheable`
//! applies, i.e. `pk` ≠ `None`) emits one `inventory::submit!` per
//! model, each containing a `SassiBootHook` whose `fn(&mut Sassi)`
//! constructs a `Punnu<T>` and registers it on the orchestrator.
//! `DjogiContext::from_pool` (and any other top-level constructor)
//! walks `inventory::iter::<SassiBootHook>()` once with a fresh
//! `&mut Sassi`, then freezes into `Arc<Sassi>` and stores it on the
//! context. After boot the registry is read-only.
//! Cross-context behaviour: each top-level `DjogiContext` builds its
//! own `Sassi`. `begin()` / `atomic(&mut pool_ctx, ...)` SHARE the parent's
//! `Arc<Sassi>` (cache state is transaction-scope-agnostic). `atomic(&pool,
//! ...)` constructs a fresh top-level transaction context because no parent
//! context was supplied. This is the "DjogiContext IS the tenant boundary"
//! contract for the cache-tenant boundary.

/// Link-time `Sassi` registration handle emitted by `#[model]`.
/// # What
/// Every `#[model]` struct (with the exception of `pk = None`) emits one
/// `inventory::submit!` of a `SassiBootHook` — a thin newtype around a
/// `fn(&mut Sassi)` registration pointer. At top-level
/// [`DjogiContext`](crate::DjogiContext) construction time
/// (`from_pool`, `from_connection`), the framework walks the inventory
/// once and applies every hook so the context's `Sassi` starts with a
/// `Punnu<T>` registered for each `Cacheable` model type compiled into
/// the binary.
/// # Why this is hidden from rustdoc
/// `SassiBootHook` is link-time machinery, not adopter-facing API.
/// Adopters should never name this type, read its inner field, or
/// construct an instance. The struct is `#[doc(hidden)]`, the inner
/// `fn` pointer is `pub(crate)`, and the
/// [`__djogi_from_model_macro`](Self::__djogi_from_model_macro)
/// constructor is `#[doc(hidden) pub]` solely so macro-emitted code in
/// downstream crates can call it through the
/// `::djogi::SassiBootHook::__djogi_from_model_macro` path (per
/// `feedback_macro_path_routing.md`). Narrowing the surface keeps the
/// framework free to evolve the inventory wiring (richer registration
/// shape, batched hooks per app, swap `inventory` for a different
/// link-time mechanism, etc.) without breaking the v0.1.0 adopter API.
/// # Adopter usage
/// Adopters interact with the framework's cache surface through the
/// `Sassi` registry built for them at context construction time
/// never by naming `SassiBootHook` directly:
/// ```rust
/// use djogi::prelude::*;
///
/// // `pub struct` matches the macro-emitted `pub` visages (`PostPublic`,
/// // `PostSelfView`, `PostAdmin`, `PostExport`) carrying `type Model =
/// // Post` per the restored Phase #231 `DjogiVisage::Model`
/// // associated-type contract. A `pub` visage referencing a private
/// // source model would trip rustc's `private_interfaces` / E0446
/// // check.
/// #[model(table = "posts")]
/// #[derive(Debug, Clone)]
/// pub struct Post {
/// pub title: String,
/// pub body: String,
/// }
///
/// fn typed_pool(ctx: &DjogiContext) {
/// // The boot hook for `Post` ran when `ctx` was constructed
/// // `ctx.punnu::<Post>` returns the registered `Arc<Punnu<Post>>`
/// // without the adopter ever naming `SassiBootHook`.
/// let _post_pool = ctx.punnu::<Post>();
/// }
/// ```
/// Macro-emitted code reaches this type through the
/// `::djogi::SassiBootHook` re-export at the crate root.
#[doc(hidden)]
pub struct SassiBootHook(pub(crate) fn(&mut sassi::Sassi));

impl SassiBootHook {
    /// Construct a boot hook from a `Sassi`-registration `fn` pointer.
    /// `#[doc(hidden) pub]` — adopter code never calls this directly.
    /// `#[model]`-emitted code in adopter crates reaches the
    /// constructor through
    /// `::djogi::SassiBootHook::__djogi_from_model_macro(...)` (per
    /// `feedback_macro_path_routing.md`); marking the field
    /// `pub(crate)` makes the implicit tuple-struct constructor
    /// unavailable to downstream macro expansion sites, which is why
    /// this named constructor exists.
    /// `const` so the macro emission can place the hook construction
    /// in `inventory::submit!`'s `static` initialiser without a runtime
    /// fence.
    #[doc(hidden)]
    pub const fn __djogi_from_model_macro(register: fn(&mut sassi::Sassi)) -> Self {
        Self(register)
    }

    /// Invoke the registration `fn` against `sassi`.
    /// `pub(crate)` — only [`DjogiContext::build_sassi`](crate::DjogiContext)
    /// walks the inventory and calls this. The runtime path stays
    /// inside the framework crate alongside the boot-time invariants
    /// (single-call-per-context, freeze-into-`Arc<Sassi>`, etc.) the
    /// runner upholds.
    pub(crate) fn run(&self, sassi: &mut sassi::Sassi) {
        (self.0)(sassi);
    }
}

inventory::collect!(SassiBootHook);
