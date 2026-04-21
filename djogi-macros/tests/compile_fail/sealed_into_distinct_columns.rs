// The `IntoDistinctColumns` trait is sealed.
//
// The trait bridges `QuerySet::distinct_on` closure returns into the
// `Vec<&'static str>` of column names the builder stores. If
// downstream crates could implement it for their own types, they
// could return a vector containing a hostile identifier that flows
// straight into `SqlAccumulator::push_sql` inside
// `djogi/src/query/sql.rs`'s `DISTINCT ON` emitters.
//
// The seal via `distinct_seal::Sealed` (crate-private module) makes
// the trait nameable as a bound but un-implementable from outside the
// djogi crate.
use djogi::query::IntoDistinctColumns;

pub struct Hostile;

fn main() {
    // This must not compile — `Sealed` is crate-private, so the impl
    // block can't name its supertrait.
    impl IntoDistinctColumns for Hostile {
        fn into_distinct_columns(self) -> Vec<&'static str> {
            vec!["1) DROP TABLE users --"]
        }
    }
}
