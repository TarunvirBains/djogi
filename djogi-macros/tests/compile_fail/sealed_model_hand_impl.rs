// The `Model` trait is sealed via `__sealed::Sealed`.
//
// Downstream crates that try to implement `Model` by hand (skipping
// `#[derive(Model)]`) fail to compile because the sealed supertrait
// is not satisfied — the `__sealed::Sealed` impl is emitted only by
// the macro. This closes the hostile-Model vector Codex flagged on
// de42874: a hand-rolled `impl Model` with a malicious `table_name()`
// or `descriptor().fields[].name` would flow straight into the SQL
// emitters otherwise.
use djogi::DjogiError;
use djogi::ModelDescriptor;
use djogi::model::Model;
use std::future::Future;

pub struct Hostile;

fn main() {
    // This must not compile — `Hostile: djogi::model::__sealed::Sealed`
    // is not satisfied, and the `__sealed` module is only reachable
    // via `#[doc(hidden)]` so there is no supported downstream path
    // to implement it. `#[derive(Model)]` is the only supported route.
    #[allow(clippy::manual_async_fn)]
    impl Model for Hostile {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "1; DROP TABLE users --"
        }
        fn pk_value(&self) -> &Self::Pk {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }
        fn get<'a>(
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
            _id: Self::Pk,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create<'a>(
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
            _v: Self,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'a>(
            &self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl Future<Output = Result<(), DjogiError>> + Send {
            async { unreachable!() }
        }
        fn delete<'a>(
            self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl Future<Output = Result<(), DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'a>(
            &self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
    }
}
