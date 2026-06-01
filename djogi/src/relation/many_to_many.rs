//! `ManyToMany<Target>` — explicit-through-model M2M relationships.
//! # What
//! A trait users impl per **direction** of a many-to-many relationship. For
//! the canonical `Person ↔ Group` pairing with an explicit `PersonGroup`
//! junction model, the "Person has many Groups" direction is
//! `impl ManyToMany<Group> for Person`, and the reverse
//! `impl ManyToMany<Person> for Group` is a separate impl. Each direction
//! reports its own [`this_fk`] / [`that_fk`] column pair, so the two sides
//! stay symmetrical without having to name the pair in a shared location.
//! The [`Through`] associated type names the junction model — itself an
//! ordinary [`Model`] queryable via `Through::objects`, just marked as a
//! through model in its [`ModelDescriptor`](crate::descriptor::ModelDescriptor)
//! (the `is_through = true` flag set by `#[model(..., through)]`). Keeping
//! `Through` queryable is deliberate: a junction row frequently carries
//! relation-specific data (a `role`, a `joined_at` timestamp, a JSONB
//! `policy` blob), and hiding those behind a generated association type
//! would force callers back into raw SQL to read them.
//! # Why no default `related` impl
//! The shape of a typed M2M query — "select `Target` rows whose PK appears
//! in `Through.{that_fk}` for rows where `Through.{this_fk} == self.pk`"
//! is trivially expressible in the framework's typed closure filter:
//! ```ignore
//! let through_rows = PersonGroup::objects()
//!     .filter(|f| f.person_id().eq(::djogi::relation::ForeignKey::new(self.id.clone())))
//!     .fetch_all(&pool).await?;
//! ```
//! but only with a compile-time-known column identifier on the `{Through}Fields`
//! handle. The trait cannot synthesise that identifier generically without
//! either (a) a stringly-typed filter escape hatch on `QuerySet<T>`
//! (rejected — the whole filter layer is typed by design), or (b) a second
//! trait indirection that promotes `this_fk` / `that_fk` from `&'static str`
//! to a field-handle type (workable but heavyweight for a single call site).
//! So `related`, [`add_related`](ManyToMany::add_related), and
//! [`remove_related`](ManyToMany::remove_related) are **required** methods
//! with no default bodies. Users who hand-write an impl write a short typed
//! body that uses the existing filter API; the upcoming `many_to_many!`
//! macro generates these impls on behalf of the user from
//! a single invocation:
//! ```ignore
//! many_to_many!(Person, Group, through = PersonGroup,
//!               this_fk = person_id, that_fk = group_id,
//!               relation = "groups");
//! ```
//! That invocation stamps out both directions. Until the macro lands, the
//! hand-written form documented on each trait method is the supported path.
//! # What the inherited seal does and does not cover
//! `ManyToMany<Target>` sits atop [`Model`], which is itself sealed via
//! [`crate::model::__sealed::Sealed`]. The `Self: Model` supertrait bound
//! restricts **which types can be the implementor**: a downstream crate
//! cannot fabricate a type that satisfies the `Model` bound without going
//! through `#[derive(Model)]` first (the sole path that emits the `Sealed`
//! impl). A hand-rolled `impl Model for Hostile { ... }` fails to compile
//! and so does a hand-rolled `impl ManyToMany<…> for Hostile`.
//! What the inherited seal does **not** cover: the **return values** of
//! [`this_fk`](ManyToMany::this_fk) / [`that_fk`](ManyToMany::that_fk) on a
//! legitimate `#[derive(Model)]`-derived implementor. Nothing stops a
//! downstream crate from writing
//! `impl ManyToMany<Group> for Person { fn this_fk -> &'static str { "id; DROP TABLE users --" } … }`
//! with a real `Person` model. The seal only refuses fake `Model`s, not
//! hostile string returns from trait methods on legitimate models.
//! **Mitigation:** any consumer that pushes `Self::this_fk` /
//! `Self::that_fk` into a SQL accumulator MUST first run the value
//! through [`crate::ident::assert_plain_ident`] (or the
//! `crate::ident::debug_assert_ident!` macro for debug-only checks). This
//! commit ships no SQL-emitting consumer of either method, so the contract
//! is documentary today; the upcoming `many_to_many!` macro
//! will validate at codegen time, and any future hand-written SQL emitter
//! must follow the same rule. Without that gate the trait would be a
//! string-based filter-bypass surface in disguise.
//! # Where
//! - [`ForeignKey<T>`](crate::relation::ForeignKey) — the FK wrapper types
//!   junction-model columns use; both FK columns on [`Through`] are
//!   `ForeignKey<Source>` / `ForeignKey<Target>` and decode the target PK
//!   via `postgres_types::FromSql`.
//! - [`QuerySet::filter`](crate::query::QuerySet::filter) — the typed
//!   closure API hand-written / macro-generated `related` bodies call
//!   into.
//! - `docs/guide/relations.md` (landing in) — user-facing
//!   guide once the macro side lands.

use crate::DjogiError;
use crate::model::Model;
use std::future::Future;

/// One side of a many-to-many relationship, pivoting through an explicit
/// junction model.
/// See the module-level docs for the full rationale — in short: this trait
/// is a typed marker-plus-contract that names the junction model and the
/// pair of FK columns joining `Self` and `Target` through it. The three
/// async methods are **required** (no default bodies) because the typed
/// filter API cannot synthesise a column handle from a `&'static str`
/// generically; hand-written impls (and the `many_to_many!`
/// macro) supply those bodies with the concrete `{Through}Fields` handle.
/// # Example
/// ```ignore
/// use djogi::prelude::*;
/// use djogi::relation::{ForeignKey, ManyToMany};
///
/// #[model(table = "persons")]
/// #[derive(Debug, Clone)]
/// pub struct Person { pub name: String }
///
/// #[model(table = "groups")]
/// #[derive(Debug, Clone)]
/// pub struct Group { pub name: String }
///
/// #[model(table = "person_groups", through, no_default)]
/// #[derive(Debug, Clone)]
/// pub struct PersonGroup {
///     pub person_id: ForeignKey<Person>,
///     pub group_id: ForeignKey<Group>,
///     pub role: String,
/// }
///
/// impl ManyToMany<Group> for Person {
///     type Through = PersonGroup;
///     const RELATION: &'static str = "groups";
///     fn this_fk() -> &'static str { "person_id" }
///     fn that_fk() -> &'static str { "group_id" }
///
///     async fn related<'ctx>(
///         &'ctx self,
///         ctx: &'ctx mut DjogiContext,
///     ) -> Result<Vec<Group>, DjogiError>
///     {
///         // ... typed-filter body; see module docs.
///         # unimplemented!()
///     }
///
///     async fn add_related<'ctx>(
///         &'ctx self,
///         ctx: &'ctx mut DjogiContext,
///         target: &'ctx Group,
///         extras: PersonGroup,
///     ) -> Result<PersonGroup, DjogiError>
///     {
///         # unimplemented!()
///     }
///
///     async fn remove_related<'ctx>(
///         &'ctx self,
///         ctx: &'ctx mut DjogiContext,
///         target: &'ctx Group,
///     ) -> Result<u64, DjogiError>
///     {
///         # unimplemented!()
///     }
/// }
/// ```
pub trait ManyToMany<Target>: Model
where
    Target: Model,
{
    /// The junction model linking `Self` and `Target`. Must itself be a
    /// `#[model(..., through)]` carrying `ForeignKey<Self>` +
    /// `ForeignKey<Target>` columns (plus any relation-specific extras).
    /// `Through` is an ordinary [`Model`] — `Through::objects` returns a
    /// fully-featured [`QuerySet<Through>`](crate::query::QuerySet) so
    /// junction-specific queries (e.g. "all admins in group X") stay in the
    /// typed layer without detouring through raw SQL.
    type Through: Model;

    /// Human-readable relation name, e.g. `"groups"` on
    /// `impl ManyToMany<Group> for Person`.
    /// Used by admin / shell tooling and by the migration-differ's audit
    /// output. Following the framework's
    /// explicit-over-implicit stance, the name is not auto-pluralised from
    /// `Target::table_name`: you pick the accessor verb.
    const RELATION: &'static str;

    /// Name of the column on [`Through`] that references `Self`'s primary
    /// key. For `impl ManyToMany<Group> for Person` with a
    /// `PersonGroup { person_id: ForeignKey<Person>, group_id: ForeignKey<Group> }`
    /// junction, this is `"person_id"`.
    fn this_fk() -> &'static str;

    /// Name of the column on [`Through`] that references `Target`'s primary
    /// key. For the `Person ↔ Group` pair above this is `"group_id"`.
    fn that_fk() -> &'static str;

    /// Return every `Target` row associated with `self` via [`Through`].
    /// The canonical hand-written body issues two queries — one against the
    /// through table collecting target PKs for rows whose `this_fk` matches
    /// `self.pk_value`, then one against `Target` with a `WHERE id IN
    /// (...)` filter. The `many_to_many!` macro emits
    /// exactly this shape on behalf of the user.
    /// # Context
    /// Takes `&'ctx mut DjogiContext`. The body re-borrows the context across
    /// the two sequential queries; `DjogiContext`'s inner variant is
    /// pattern-matched at each query dispatch boundary in the emitted code.
    fn related<'ctx>(
        &'ctx self,
        ctx: &'ctx mut crate::context::DjogiContext,
    ) -> impl Future<Output = Result<Vec<Target>, DjogiError>> + Send + 'ctx;

    /// Attach `self` to `target` by inserting a row into [`Through`].
    /// `extras` carries the junction-model-specific columns (e.g. a
    /// `role: String` or a `joined_at: DateTime`). The canonical hand-written
    /// body overwrites `extras.{this_fk}` and `extras.{that_fk}` with
    /// freshly-constructed [`ForeignKey`](crate::relation::ForeignKey)
    /// values built from `self.pk_value` and `target.pk_value`, then
    /// calls `Through::create(ctx, extras)`. That the caller supplies
    /// the whole junction row by value keeps the junction-specific fields
    /// under the user's control — the framework does not invent values for
    /// non-FK columns.
    /// Returns the freshly-created junction row with its
    /// framework-populated `id` / `created_at` / `updated_at`.
    fn add_related<'ctx>(
        &'ctx self,
        ctx: &'ctx mut crate::context::DjogiContext,
        target: &'ctx Target,
        extras: Self::Through,
    ) -> impl Future<Output = Result<Self::Through, DjogiError>> + Send + 'ctx;

    /// Detach `self` from `target` by deleting the matching junction row.
    /// The canonical hand-written body filters
    /// `Through::objects` on both `this_fk == self.pk` and
    /// `that_fk == target.pk`, then calls `.delete(ctx)` — yielding
    /// the number of rows deleted (typically `0` or `1`, depending on
    /// whether the junction had a `UNIQUE (this_fk, that_fk)` constraint).
    /// A higher count indicates either a missing uniqueness constraint on
    /// the junction table or a data-integrity breach; callers that need to
    /// enforce "exactly one removed" should inspect the returned count.
    fn remove_related<'ctx>(
        &'ctx self,
        ctx: &'ctx mut crate::context::DjogiContext,
        target: &'ctx Target,
    ) -> impl Future<Output = Result<u64, DjogiError>> + Send + 'ctx;
}
