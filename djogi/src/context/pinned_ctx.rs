use super::DjogiContext;

/// Return type of [`DjogiContext::pin_for_migration`](super::DjogiContext::pin_for_migration).
///
/// Derefs to `&mut DjogiContext` so callers can pass it to any
/// function that accepts `&mut PinnedCtx<'_>`. When the migration
/// entry point drops the `PinnedCtx`, the `Owned` variant's
/// checked-out connection is returned to the pool and the session
/// is implicitly released.
///
/// # Structural invariant
///
/// The enum variants are `pub(crate)` (Rust does not support independent
/// variant visibility — they inherit the enum's visibility). Construction
/// is restricted to this module tree by convention and code review. Only
/// [`DjogiContext::pin_for_migration`](super::DjogiContext::pin_for_migration)
/// constructs these variants (GH #331 Finality F-331-1).
#[allow(clippy::large_enum_variant)]
pub(crate) enum PinnedCtx<'a> {
    /// Pool-backed context: a fresh connection was checked out and
    /// wrapped in a new `DjogiContext`. Dropping this variant
    /// returns the connection to the pool (and closes the session,
    /// implicitly releasing any advisory lock).
    Owned(DjogiContext),
    /// Already connection-backed: borrows the caller's context.
    Borrowed(&'a mut DjogiContext),
}

impl<'a> std::ops::Deref for PinnedCtx<'a> {
    type Target = DjogiContext;

    fn deref(&self) -> &DjogiContext {
        match self {
            PinnedCtx::Owned(ctx) => ctx,
            PinnedCtx::Borrowed(ctx) => ctx,
        }
    }
}

impl<'a> std::ops::DerefMut for PinnedCtx<'a> {
    fn deref_mut(&mut self) -> &mut DjogiContext {
        match self {
            PinnedCtx::Owned(ctx) => ctx,
            PinnedCtx::Borrowed(ctx) => ctx,
        }
    }
}
