// Phase 8eta PR3 — adopter custom scalar opts into direct portable ordering.
//
// PR3 deliberately does NOT ship a blanket `impl<T: PartialOrd + ToSql + ...>
// DjogiPortableOrd for T` because that would silently include `Option<U>`
// (whose Rust ordering doesn't match SQL three-valued NULL semantics) and
// any future foreign type. The trait is sealed by absence of a blanket
// — adopters opt their custom scalar types in explicitly with
// `impl djogi::query::DjogiPortableOrd for MyType {}` once they also
// satisfy `postgres_types::ToSql + Clone + Send + Sync + 'static`.
//
// This fixture exercises the canonical extension shape: a `Rank(i32)`
// newtype that binds through `postgres_types::ToSql`, opts into the
// marker trait, and is consumed inside a portable predicate via
// `f.rank().gte(Rank(3))`. The compile-pass success proves the
// extension surface is genuinely public and locks down the bound shape
// adopters depend on.
//
// `Rank` is intentionally NOT a `#[model]` field type here — that would
// require additional adopter plumbing (`Default` / `DjogiSqlType` /
// `FromSql` impls) that aren't part of the PR3 surface contract.
// Instead, `__make_djogi_field` is invoked directly through the same
// macro-support entry point PR3 wires into generated `{Model}Fields`
// accessors, keeping the bound assertion focused on the
// `DjogiPortableOrd` opt-in.
//
// Per the lihaaf compile-fixture contract, every lihaaf fixture has
// `fn main` so the binary still has to link.

use djogi::__private::query::__make_djogi_field;
use djogi::__private::{bytes, postgres_types};
use djogi::prelude::*;
use djogi::query::{DjogiField, PortablePredicate};

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
struct Rank(i32);

impl postgres_types::ToSql for Rank {
    fn to_sql(
        &self,
        ty: &postgres_types::Type,
        out: &mut bytes::BytesMut,
    ) -> Result<postgres_types::IsNull, Box<dyn std::error::Error + Send + Sync>> {
        <i32 as postgres_types::ToSql>::to_sql(&self.0, ty, out)
    }

    fn accepts(ty: &postgres_types::Type) -> bool {
        <i32 as postgres_types::ToSql>::accepts(ty)
    }

    postgres_types::to_sql_checked!();
}

impl djogi::query::DjogiPortableOrd for Rank {}

#[model(table = "phase8eta_custom_scalar_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
    pub label: String,
}

fn main() {
    // Construct a `DjogiField<Widget, Rank>` directly through the
    // macro-support entry point — same path the post-PR3 generated
    // `{Model}Fields` accessors take. The compile-pass success proves
    // `DjogiPortableOrd` is satisfied for `Rank` and that the
    // ordering-bounded `gte` impl block resolves to a `Rank` payload.
    let rank_field: DjogiField<Widget, Rank> = __make_djogi_field("rank", |w| {
        // Borrow a `&Rank` derived from one of `Widget`'s real fields.
        // The closure body is irrelevant for the type-check — the
        // fixture asserts the predicate-build surface only — but
        // returning a reference of the right type keeps the function
        // pointer signature `fn(&Widget) -> &Rank` honest.
        // Using `transmute` here would invoke UB; instead we hold an
        // immortal default and return its reference. `Rank::default()`
        // is unavailable, so we leak a static `Rank` once.
        static RANK_FALLBACK: std::sync::OnceLock<Rank> = std::sync::OnceLock::new();
        let _ = &w.label; // touch the model so the closure isn't a no-op.
        RANK_FALLBACK.get_or_init(|| Rank(0))
    });

    // `gte` lives in the `V: DjogiPortableOrd` extension impl. The
    // compile-pass shape proves the opt-in trait is genuinely public
    // and the bound shape adopters depend on locks down.
    let _pred: PortablePredicate<Widget> = rank_field.gte(Rank(3));
}
