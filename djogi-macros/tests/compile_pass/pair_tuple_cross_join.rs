// typed pair-tuple compile-pass: two-model cross-join.
//
// Locks the entry point for the heterogeneous cross-join shape:
//
//   QuerySet<L>::cross_join_with::<R>(qs: QuerySet<R>) -> JoinedQuerySet<L, R>
//
// `cross_join_with` does NOT default to the `l.id <> r.id` filter — the
// PK namespaces for unrelated models are unlikely to overlap, and
// silently dropping rows where they do would be a footgun. Adopters
// wanting the anti-equality filter (e.g. for self-join shapes through
// `cross_join_with` on `L = R`) call `.self_pairs()` instead.
//
// Pins issue #99's "two-Model cross-join" coverage at the type level.

use djogi::prelude::*;

#[model(table = "phase8_5_cross_join_orders")]
#[derive(Debug, Clone)]
pub struct Order {
    pub customer_name: String,
    pub total_cents: i64,
}

#[model(table = "phase8_5_cross_join_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
    pub sku: String,
    pub price_cents: i64,
}

fn main() {
    // Heterogeneous cross-join: every Order × every Widget. The two
    // sides retain their own filter / ordering accumulators; the
    // pair-side builder methods AND additional clauses on top.
    let _pairs: JoinedQuerySet<Order, Widget> = Order::objects()
        .filter(|o| o.total_cents().gte(100i64))
        .cross_join_with(Widget::objects().filter(|w| w.price_cents().lt(50i64)));

    // Pair-side filter AND-ing after the cross-join is also valid;
    // `filter_left` / `filter_right` thread back into the underlying
    // QuerySet's condition so the two paths produce equivalent SQL.
    let _composed: JoinedQuerySet<Order, Widget> = Order::objects()
        .cross_join_with(Widget::objects())
        .filter_left(|o| o.total_cents().gt(0i64))
        .filter_right(|w| w.sku().contains("WIDGET"));

    // The cross-join builder does not enable the anti-equality filter
    // by default. Calling `.include_equal_pk()` on a cross-join is
    // currently a no-op (the flag was already false), but the method
    // exists so that callers can opt into the same builder API
    // surface for both self_pairs and cross_join_with results.
    let _explicit_include: JoinedQuerySet<Order, Widget> = Order::objects()
        .cross_join_with(Widget::objects())
        .include_equal_pk();

    // Pair-tuple pagination still flows through here — the LIMIT /
    // OFFSET apply to the pair result set, not to either side.
    let _paged: JoinedQuerySet<Order, Widget> = Order::objects()
        .cross_join_with(Widget::objects())
        .limit(100)
        .offset(20);
}
