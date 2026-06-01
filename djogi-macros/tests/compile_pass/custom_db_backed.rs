// `djogi::primary_key!` emits a DB-backed custom PK
// when `bulk_sql = "..."` is set. The `#[model(pk = MyAppId)]` attribute
// accepts the newtype by name and wires the injected `id` field through
// `MyAppId`'s `PrimaryKey` impl.

use djogi::prelude::*;

djogi::primary_key! {
    pub struct MyAppId(i64);
    sql_type = "BIGINT";
    default_sql = "my_app_id_next()";
    bulk_sql = "SELECT id FROM my_app_id_next_many($1)";
}

#[model(table = "orders", pk = MyAppId)]
#[derive(Debug, Clone)]
pub struct Order {
    pub total_cents: i64,
}

fn _custom_pk_surface(o: &Order) {
    // Injected `id` is typed as the user's newtype — not a built-in.
    let _: &MyAppId = &o.id;
    // The `PrimaryKey` associated consts read back at compile time.
    const _KIND: ::djogi::PkType = <MyAppId as ::djogi::primary_key::PrimaryKey>::KIND;
    const _SQL_TYPE: &str = <MyAppId as ::djogi::primary_key::PrimaryKey>::SQL_TYPE;
    const _DEFAULT_SQL: ::std::option::Option<&str> =
        <MyAppId as ::djogi::primary_key::PrimaryKey>::DEFAULT_SQL;
}

fn main() {}
