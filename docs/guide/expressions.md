> [Back to Guides](./index.md) · [Back to README](../../README.md)

# Expressions

Phase 4's expression IR: typed `Expr<T>`, arithmetic, field-vs-field,
CASE/WHEN, correlated subqueries, aggregates, and annotations. One IR
feeds filters, SET assignments, and aggregate terminals — no parallel
SQL assembler.

## `Expr<T>`

`Expr<T>` wraps an internal `ExprNode` with a phantom type `T`. The
`T` generic is the column's Rust type — `i64`, `String`, `Decimal`,
etc. Operators on `Expr<T>` compose while preserving `T`:

- `Expr<T> + Expr<T>` (and `-`, `*`, `/`) for `T: Numeric`
- `expr.eq(other)` / `.gt` / `.lt` / etc. → `Expr<bool>`
- `case().when(cond, then_val).otherwise(else_val)` — typestate
  builder that terminates at `.otherwise()` into `Expr<T>`

Field handles lift into expressions via `.as_expr()`:

```rust
use djogi::prelude::*;

// Filter: balance > minimum_balance (field-vs-field)
Account::objects().filter(|f| f.balance().as_expr().gt(f.minimum_balance().as_expr()))
```

Literals lift via `IntoExpr` (implemented for every `IntoFilterValue`
type).

## Expression-backed UPDATE

`FieldRef::set(expr)` accepts an `Expr<T>` payload, letting assignments
reference other columns:

```rust
// SET view_count = view_count + 1
Post::objects()
    .filter(|f| f.id().eq(post_id))
    .update(|f| f.view_count().set(f.view_count().as_expr() + Expr::from(1i64)))
    .execute(ctx).await?;
```

## Aggregates + annotations

Five aggregate operations — `count`, `sum`, `avg`, `min`, `max` — each
returns an `AggregateExpr<Out>`. `.filter(cond)` adds a
`FILTER(WHERE ...)` tail.

Terminal:

```rust
let total: i64 = Order::objects()
    .aggregate(|f| f.amount().sum())
    .fetch_one(ctx).await?;
```

Annotations attach per-row aggregate columns via `QuerySet::annotate`
and return a typed tuple:

```rust
// Vec<(User, i64)> — each user + their order count
let rows: Vec<(User, i64)> = User::objects()
    .annotate(|f| f.orders().count())
    .fetch_all(ctx).await?;

// Arity 2+: nested tuples
let rows: Vec<(User, (i64, Decimal))> = User::objects()
    .annotate(|f| (f.orders().count(), f.orders().sum(|o| o.amount())))
    .fetch_all(ctx).await?;
```

`IntoAggregateTuple` is sealed; implementations exist for arity 1
through 4.

## Subqueries + EXISTS + OuterRef

`Subquery<T, V>` wraps a scalar-returning queryset. Typed
`OuterRef<M, V>` references the outer query's columns in a correlated
subquery.

```rust
// Posts whose author has ≥ 10 followers.
let subquery = Author::objects()
    .filter_expr(|f| {
        f.id()
            .as_expr()
            .eq(PostOuterRef::author_id().as_qualified_expr())
    })
    .filter(|f| f.follower_count().gte(10i64));

Post::objects()
    .filter_expr(|_| Exists::new(subquery).as_expr())
    .fetch_all(ctx).await?;
```

`PostOuterRef::author_id()` is the macro-emitted helper. It returns a
typed outer reference bound to the enclosing `Post` query; the explicit
`.as_qualified_expr()` form avoids ambiguity when both scopes expose an
`id` column.

Visage querysets reuse the same substrate. `IN` and `NOT IN` project one
explicit exposed column:

```rust
use djogi::prelude::*;

let gold_authors = AuthorPublic::filter(|a| a.tier().eq("gold".to_string()))
    .selecting(AuthorPublic::id())?;

Post::objects()
    .filter(|f| f.author_id().in_visage(gold_authors))
    .fetch_all(&mut ctx).await?;
```

`EXISTS` over a visage queryset uses `VisageExists` and keeps the same
outer-ref pattern:

```rust
use djogi::prelude::*;

let has_published = VisageExists::new(PostPublic::filter(|p| {
    Q::Expression(p.published().as_expr().eq(Expr::literal(true)))
        & Q::Expression(
            p.author_id()
                .as_expr()
                .eq(AuthorOuterRef::id().as_qualified_expr()),
        )
}))?;

Author::objects()
    .filter(|_| has_published)
    .fetch_all(&mut ctx).await?;
```

`selecting(...)` and `VisageExists::new(...)` are both fallible: they reject
`order_by` / `limit` / `offset` carried on a `VisageQuerySet`, because those
modifiers do not survive subquery lowering and Djogi refuses to drop them
silently.

## CASE / WHEN

```rust
// SET status = CASE WHEN balance < 0 THEN 'overdrawn' ELSE 'ok' END
Account::objects()
    .update(|f| f.status().set(
        case()
            .when(f.balance().as_expr().lt(Expr::from(0i64)), Expr::from("overdrawn".to_string()))
            .otherwise(Expr::from("ok".to_string()))
    ))
    .execute(ctx).await?;
```

The `CaseBuilder<V>` typestate rejects `.otherwise()` before any
`.when()` (compile error on `CaseBuilder<Empty>`) and rejects `.when()`
calls whose `then` type differs from earlier arms.
