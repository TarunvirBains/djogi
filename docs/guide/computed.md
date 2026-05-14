# Computed Properties

> **Phase**: 8 Cluster 8β — Proxy + Computed Properties
> **Status**: v0.1.0 (SQL-projectable half + Rust-trait registration)
> **Spec anchor**: `docs/spec/implementation-plan.md` §637–650

A **computed property** is a virtual model field whose value is derived from
other columns at query time, not stored as its own column. Computed
properties come in two flavours:

- **SQL-projectable** — defined via `#[computed(sql = "...")]` on a struct
  field. Used in `.annotate()`, `.filter_expr()`, and `.order_by()` through
  the `{Model}Computed` ZST. The expression evaluates server-side; no
  storage column is allocated.
- **Rust-trait** — defined by implementing a Rust trait via
  `#[djogi::trait_impl] impl Trait for Model { ... }`. Used for
  cross-cutting predicates that depend on Rust logic rather than SQL
  expressions.

This guide covers both, with a section at the end on how to choose between
them.

## SQL-projectable computed fields

### Declaring

Annotate a virtual field with `#[computed(sql = "...")]`:

```rust
use djogi::prelude::*;

#[model(table = "vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub base_price: f64,
    pub tax_rate: f64,

    #[computed(sql = "base_price * (1.0 + tax_rate)")]
    pub total_price: f64,
}
```

The macro:

1. Removes the computed field from the struct entirely — no `total_price`
   column on the Rust struct, no slot in `Vehicle::COLUMN_LIST`, no entry
   in the migration differ's column set, no INSERT/UPDATE binding. The
   field name lives on only inside the macro inputs and the SQL-side ZST
   accessor described below.
2. Emits a `{Model}Computed` ZST with a typed accessor returning
   `Expr<f64>`: adopters call `Vehicle::computed().total_price()` to thread
   the computed expression through the typed query API.
3. Records the field in `ModelDescriptor.computed_fields` for `djogi docs`
   to render alongside regular columns.

### Using in queries

The `{Model}Computed` ZST returns `Expr<V>` values that compose with the
existing `Expr<T>` API.

The supported v0.1.0 terminal is `QuerySet::filter_expr` — it accepts a
closure returning `Expr<bool>`, and the comparison methods on `Expr<T>`
(`.gte` / `.lt` / `.eq` / etc.) produce that boolean expression by pairing
the computed accessor with a literal:

```rust
// Filter by a computed expression:
Vehicle::objects()
    .filter_expr(|_| Vehicle::computed().total_price().gte(Expr::literal(100.0_f64)))
    .fetch_all(&mut ctx).await?;
```

The emitted SQL splices the user-authored fragment in outer parens at every
emission site for operator-precedence stability:

```sql
SELECT * FROM vehicles WHERE (base_price * (1.0 + tax_rate)) >= $1
```

**Ordering and annotation by a computed expression are deferred to a
later task.** `QuerySet::order_by` accepts a closure returning
`OrderExpr` (built from `FieldRef::asc` / `FieldRef::desc`), and
`QuerySet::annotate` accepts a closure returning an `AggregateExpr`
tuple. Neither surface accepts a bare `Expr<T>` today — there is no
`From<Expr<T>>` impl for `OrderExpr`, and `AnnotationSlot` is sealed on
`AggregateExpr<V>` and the window-function families. Adopters who need
to sort or project by a computed expression in v0.1.0 use a justified
raw-SQL bypass (`DjogiContext::raw_query` under
`#[djogi::deliberately_bypass_convention_with_raw_sql]` plus an adjacent
`// JUSTIFICATION ...` comment) with the SQL fragment inlined.

A future cluster (T6's `Q<T>` algebra refactor in 8γ, or the
follow-up surface task tracked alongside the `{Model}Computed` ZST)
will widen `OrderExpr` and `AnnotationSlot` to admit arbitrary
`Expr<T>` so the two terminals close the loop without an escape hatch.

### Rust-side evaluation path

The macro does **not** emit a Rust-side getter for computed fields. The
canonical surface is the SQL-projectable ZST: `Vehicle::computed()
.total_price()` returns `Expr<f64>` and composes with the typed query
API. For server-side evaluation that is the entire story — adopters
who only need to filter / sort / annotate by a computed expression pay
no Rust-side cost.

If you need to evaluate the expression in Rust (e.g. inside a hook or a
visage `try_from`), write a plain inherent method on your struct with
any name that suits the call site:

```rust
impl Vehicle {
    pub fn total_price(&self) -> f64 {
        self.base_price * (1.0 + self.tax_rate)
    }
}
```

The hand-written method does not collide with anything macro-emitted —
no auto-derived stub exists to override.

**Why no auto-derived getter?** Per the project's design lens
(`feedback_decision_priorities.md`, plan §7 #8 resolved 2026-05-03):
production stability wins over auto-derivation for the narrow CASE/WHEN
/ function-call tail. A home-grown SQL→Rust arithmetic translator
would ship bug-for-bug copies of Postgres semantics — rounding
divergence between Rust `f64` and Postgres `numeric`, NULL-coalescing
edge cases, integer overflow rules. The earlier shape emitted an
`unimplemented!()` stub on the assumption that Rust's inherent-impl
resolution would prefer a hand-written override; that premise is
incorrect — Rust does not allow two inherent methods with the same name
on the same type (E0201). Removing the stub honors the lens resolution
without the duplicate-method footgun.

### Constraint: stored variant deferred to Phase 8.5

`#[computed(sql = "...", stored)]` (a stored generated column whose value is
materialised in storage) is **rejected** at parse time with a Phase 8.5
deferral message. The migration differ has not yet accumulated long-running
stability evidence post-publish, so generating column DDL from a `stored`
computed is out of scope for v0.1.0.

Adopters who need stored computed columns in v0.1.0 can:

- Ship a non-stored computed for now; revisit when the deferral lifts.
- Hand-roll a regular column + a `BEFORE INSERT/UPDATE` trigger via raw SQL
  in a migration. The framework does not auto-generate the trigger.

## Rust-trait registration

For cross-cutting predicates that depend on Rust logic — full-text search
adapters, custom serialisation drivers, type-erased dispatch — declare a
trait and register implementing models via `#[djogi::trait_impl]`:

```rust
use djogi::prelude::*;

trait Searchable {
    fn searchable_columns(&self) -> &'static [&'static str];
}

#[model(table = "vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub title: String,
    pub description: String,
}

#[djogi::trait_impl]
impl Searchable for Vehicle {
    fn searchable_columns(&self) -> &'static [&'static str] {
        &["title", "description"]
    }
}
```

The macro emits the impl block verbatim plus an
`inventory::submit!(TraitRegistration { ... })` block. Cross-cutting
consumers walk the registry via `djogi::trait_registry::iter_for_trait::<dyn
Searchable>()` to enumerate every model that implements the trait, without
hard-coding model names in the consumer's path.

### Constraints

- Trait impls only — inherent `impl Type { ... }` rejected.
- Concrete impls only — `impl<T> Trait for Vec<T>` rejected (generic impls
  are deferred to a future phase).
- The self-type must be a named type (`Vehicle`, `crate::module::Vehicle`),
  not a tuple, reference, or function-pointer type.

### Sassi integration

If you have `sassi` enabled and your models are Punnu-pooled, prefer
`#[sassi::trait_impl]` and `Sassi::all_impl::<dyn T>()` for the full
sassi-integrated cross-type query path. Sassi's registry is keyed off
`Punnu<T>` boundaries and constructs `Vec<Arc<dyn T>>` across every
Punnu-registered model.

`#[djogi::trait_impl]` is the sibling surface for adopters who need
descriptor-level enumeration outside the Punnu boundary.

## When to choose SQL-projectable vs Rust-trait

| Question | Answer |
|----------|--------|
| Does the predicate depend only on existing model columns? | SQL-projectable |
| Does the predicate need Rust-side runtime state (request context, environment, etc.)? | Rust-trait |
| Does the predicate need to filter / sort at the database? | SQL-projectable |
| Does the predicate need cross-type dispatch over `Vec<Arc<dyn T>>`? | Rust-trait |
| Is the expression a single arithmetic / comparison operation? | SQL-projectable |
| Is the expression a sequence of complex transformations? | Rust-trait |

The two are not mutually exclusive — a model can have both
`#[computed(sql)]` fields AND `#[djogi::trait_impl]` registrations.

## Related documents

- [Models](models.md) — base `#[model(...)]` attribute reference.
- [Expressions](expressions.md) — typed `Expr<T>` API.
- [Proxy Models](proxy.md) — the sibling chapter from the same cluster.
