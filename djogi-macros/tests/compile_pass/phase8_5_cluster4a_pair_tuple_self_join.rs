// Phase 8.5 Cluster 4A — typed pair-tuple compile-pass: self-join shape.
//
// Locks the entry-point types and the build-time return-type wiring:
//
//   QuerySet<Widget>::self_pairs() -> JoinedQuerySet<Widget, Widget>
//   JoinedQuerySet<Widget, Widget>::filter_left / filter_right    -> Self
//   JoinedQuerySet<Widget, Widget>::order_by_left / order_by_right -> Self
//   JoinedQuerySet<Widget, Widget>::include_equal_pk              -> Self
//   JoinedQuerySet<Widget, Widget>::limit / offset                -> Self
//
// Pins issue #99's "single-model self-join" coverage at the type level.
// Runtime emission is validated by `query::joined::tests` and the
// integration suite under `tests/integration/phase8_5_cluster4a_*.rs`.
//
// Lihaaf compile-pass fixtures need `fn main()` so they link cleanly.

use djogi::prelude::*;

#[model(table = "phase8_5_pair_tuple_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
    pub price: i64,
}

fn main() {
    // Self-join with the default exclude_equal_pk (l.id <> r.id) — the
    // canonical "every distinct ordered pair" shape used by the
    // mating-pairs demo's candidate generation.
    let _pairs: JoinedQuerySet<Widget, Widget> = Widget::objects()
        .filter(|w| w.price().gte(10i64))
        .self_pairs();

    // Per-side typed filters AND onto each underlying QuerySet's
    // condition before the cross-join emits `WHERE` clauses qualified
    // by side alias (`l.` / `r.`).
    let _filtered: JoinedQuerySet<Widget, Widget> = Widget::objects()
        .self_pairs()
        .filter_left(|w| w.price().gte(100i64))
        .filter_right(|w| w.price().lt(500i64));

    // Per-side ordering — appended, alias-qualified at emit time.
    let _ordered: JoinedQuerySet<Widget, Widget> = Widget::objects()
        .self_pairs()
        .order_by_left(|w| w.name().asc())
        .order_by_right(|w| w.price().desc());

    // Opt-in to including the identity row (`l.id = r.id`) — useful for
    // unordered-pair semantics where pairing a row with itself is
    // legitimate.
    let _with_identity: JoinedQuerySet<Widget, Widget> =
        Widget::objects().self_pairs().include_equal_pk();

    // Pair-tuple pagination — replaces any prior call. Separate from
    // per-side limits (which the joined builder ignores).
    let _paged: JoinedQuerySet<Widget, Widget> =
        Widget::objects().self_pairs().limit(50).offset(0);
}
