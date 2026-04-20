> [Back to Guides](./index.md) · [Back to README](../../ReadMe.MD)

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
// Posts whose author has ≥ 10 followers
let subquery = Author::objects()
    .filter(|f| f.id().eq_outer(OuterRef::<Post, _>::author_id()))
    .filter(|f| f.follower_count().gte(10i64));

Post::objects()
    .filter(|_| Exists::new(subquery))
    .fetch_all(ctx).await?;
```

`OuterRef<Post, HeerId>::author_id()` is a macro-emitted associated
function returning a typed reference to `Post.author_id` bound for
use inside a subquery's `filter`. Mismatched value types fail at
compile time.

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
