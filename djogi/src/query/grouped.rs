//! Grouped query state types. See Phase 6.5 plan for the type-state
//! contract and method legality table.
//!
//! # Type-state transitions
//!
//! - `QuerySet<T>` → `GroupedQuerySet<T, K>` via `.group_by`
//! - `GroupedQuerySet<T, K>` → `GroupedAnnotatedQuerySet<T, K, A>` via `.annotate`
//! - `GroupedAnnotatedQuerySet<T, K, A>` is the only state with terminals
//!   (`.fetch_all`, `.stream`)
//!
//! Premature `.fetch_all` on `GroupedQuerySet<T, K>` is a compile error
//! (no such method exists). This is enforced structurally rather than via
//! runtime checks.

use crate::model::Model;
use crate::pg::accumulator::SqlAccumulator;
use crate::query::field::FieldRef;

mod sealed {
    pub trait Sealed {}
}

/// Typed group-key tuple — produced by the closure passed to
/// `QuerySet::group_by`. Arity 1..=4 supported; wider shapes steer
/// users to a local struct post-hoc (same policy as annotate).
pub trait IntoGroupKeyTuple: sealed::Sealed {
    /// The Rust tuple the terminal decodes each grouped row into.
    type Decoded;

    /// Emit the column list for the `GROUP BY` clause onto `acc`.
    fn push_group_by_columns(&self, acc: &mut SqlAccumulator);

    /// Emit the same column list as the SELECT-list leading columns.
    fn push_select_columns(&self, acc: &mut SqlAccumulator);

    /// Decode the N leading columns of a grouped row into `Decoded`.
    fn decode_tuple(row: &tokio_postgres::Row) -> Result<Self::Decoded, tokio_postgres::Error>;
}

// ── Arity 1: single FieldRef<M, V> ───────────────────────────────────────

impl<M: Model, V> sealed::Sealed for FieldRef<M, V> {}

impl<M: Model, V> IntoGroupKeyTuple for FieldRef<M, V>
where
    V: for<'a> postgres_types::FromSql<'a> + Send + Unpin + 'static,
{
    type Decoded = V;

    fn push_group_by_columns(&self, acc: &mut SqlAccumulator) {
        acc.push_sql(self.column());
    }

    fn push_select_columns(&self, acc: &mut SqlAccumulator) {
        acc.push_sql(self.column());
    }

    fn decode_tuple(row: &tokio_postgres::Row) -> Result<Self::Decoded, tokio_postgres::Error> {
        row.try_get::<_, V>(0)
    }
}

// ── Arity 2..=4: tuples of FieldRef<M, V_i> ──────────────────────────────

macro_rules! impl_into_group_key_tuple {
    (
        arity = $arity:tt,
        types = [ $( ($ty:ident, $slot:tt, $pos:literal) ),+ $(,)? ]
    ) => {
        impl<M: Model, $($ty),+> sealed::Sealed for ( $(FieldRef<M, $ty>,)+ ) {}

        impl<M: Model, $($ty),+> IntoGroupKeyTuple for ( $(FieldRef<M, $ty>,)+ )
        where
            $( $ty: for<'a> postgres_types::FromSql<'a> + Send + Unpin + 'static, )+
        {
            type Decoded = ( $($ty,)+ );

            fn push_group_by_columns(&self, acc: &mut SqlAccumulator) {
                let mut first = true;
                $(
                    if !first { acc.push_sql(", "); }
                    first = false;
                    acc.push_sql(self.$slot.column());
                )+
                let _ = first;
            }

            fn push_select_columns(&self, acc: &mut SqlAccumulator) {
                let mut first = true;
                $(
                    if !first { acc.push_sql(", "); }
                    first = false;
                    acc.push_sql(self.$slot.column());
                )+
                let _ = first;
            }

            fn decode_tuple(row: &tokio_postgres::Row) -> Result<Self::Decoded, tokio_postgres::Error> {
                Ok((
                    $( row.try_get::<_, $ty>($pos)?, )+
                ))
            }
        }
    };
}

impl_into_group_key_tuple!(arity = 2, types = [(A, 0, 0), (B, 1, 1)]);
impl_into_group_key_tuple!(arity = 3, types = [(A, 0, 0), (B, 1, 1), (C, 2, 2)]);
impl_into_group_key_tuple!(
    arity = 4,
    types = [(A, 0, 0), (B, 1, 1), (C, 2, 2), (D, 3, 3)]
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::ModelDescriptor;

    // Inert local model — mirrors the stub used across annotate.rs tests.
    struct Fake;
    impl crate::model::__sealed::Sealed for Fake {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Fake {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "fakes"
        }
        fn pk_value(&self) -> &i64 {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
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

    // Step 1.2 — arity-1
    #[test]
    fn arity_one_field_ref_implements_into_group_key_tuple() {
        fn assert_bound<T: IntoGroupKeyTuple>() {}
        assert_bound::<FieldRef<Fake, i64>>();
    }

    // Step 1.3 — arity 2..=4
    #[test]
    fn arity_two_tuple_implements_into_group_key_tuple() {
        fn assert_bound<T: IntoGroupKeyTuple>() {}
        assert_bound::<(FieldRef<Fake, i64>, FieldRef<Fake, String>)>();
    }

    #[test]
    fn arity_three_tuple_implements_into_group_key_tuple() {
        fn assert_bound<T: IntoGroupKeyTuple>() {}
        assert_bound::<(
            FieldRef<Fake, i64>,
            FieldRef<Fake, String>,
            FieldRef<Fake, i32>,
        )>();
    }

    #[test]
    fn arity_four_tuple_implements_into_group_key_tuple() {
        fn assert_bound<T: IntoGroupKeyTuple>() {}
        assert_bound::<(
            FieldRef<Fake, i64>,
            FieldRef<Fake, String>,
            FieldRef<Fake, i32>,
            FieldRef<Fake, bool>,
        )>();
    }
}
