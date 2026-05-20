// Phase 8.5 Cluster 4 (djogi#216) Piece A — `#[field(domain = "<name>")]`
// attribute references an adopter-managed Postgres domain.
//
// Exercises the macro's parse + lower path for the new domain-reference
// surface:
//
// 1. A simple `Decimal` field with `domain = "positive_amount"` lowers to
//    `FieldSqlType::Domain { name: "positive_amount", base: &FieldSqlType::Numeric }`
//    in the emitted descriptor.
// 2. A `String` field with `domain = "email_address"` lowers to
//    `FieldSqlType::Domain { name: "email_address", base: &FieldSqlType::Text }`.
// 3. Nullable + domain composes — `Option<rust_decimal::Decimal>` carrying
//    `domain = "positive_amount"` produces the same domain shape with
//    `nullable: true`.
// 4. `domain + check` is allowed (the adopter CHECK ANDs into the
//    constraint slot that the domain's own constraints already populate
//    on the database side).
// 5. `domain + type_change_using` is allowed (the USING expression
//    drives a one-time migration from another column type to the
//    domain).
// 6. Domain names that shadow Postgres built-in type keywords (`text`,
//    `integer`, `select`) are accepted — domain identifiers are SQL
//    type names, not column / table identifiers, and `CREATE DOMAIN
//    "text" AS varchar(64)` is legitimate (if confusing) SQL.
//
// `no_default` because the surface here does not require user-supplied
// `Default` for the framework-injected columns.

use djogi::prelude::*;

// ── (1) Decimal field with `domain = "positive_amount"` ──────────────────

#[model(table = "orders_216", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Order216 {
    #[field(domain = "positive_amount")]
    pub amount: rust_decimal::Decimal,
}

// ── (2) String field with `domain = "email_address"` ─────────────────────

#[model(table = "users_216", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct User216 {
    #[field(domain = "email_address")]
    pub email: String,
}

// ── (3) Nullable + domain ────────────────────────────────────────────────

#[model(table = "invoices_216", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Invoice216 {
    /// Nullable domain field — the `Option<…>` wrapper lifts cleanly
    /// onto the domain reference. The descriptor carries
    /// `nullable: true` alongside the `Domain` sql_type.
    #[field(domain = "positive_amount")]
    pub amount: Option<rust_decimal::Decimal>,
}

// ── (4) `domain + check` — both apply, ANDed at projection ───────────────

#[model(table = "ledgers_216", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Ledger216 {
    /// Adopter CHECK layered on a domain. The domain's own constraints
    /// (declared via `CREATE DOMAIN positive_amount AS NUMERIC CHECK
    /// (VALUE > 0)` on the database side) provide the lower bound; the
    /// adopter's `#[field(check = "amount < 1000000")]` adds an upper
    /// bound on this specific column without modifying the shared
    /// domain definition.
    #[field(domain = "positive_amount", check = "amount < 1000000")]
    pub amount: rust_decimal::Decimal,
}

// ── (5) `domain + type_change_using` — one-time migration directive ──────

#[model(table = "receipts_216", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Receipt216 {
    /// Migrating an existing `NUMERIC` column to a domain reference.
    /// The USING expression drives the one-time `ALTER COLUMN … TYPE
    /// positive_amount USING (amount)` clause on the diff that
    /// introduces the domain reference.
    #[field(domain = "positive_amount", type_change_using = "amount")]
    pub amount: rust_decimal::Decimal,
}

// ── (6) Domain name shadowing built-in keyword ───────────────────────────

#[model(table = "shadow_216", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Shadow216 {
    /// `text` is a Postgres built-in type, but it is also a legal
    /// adopter-defined domain identifier. The macro's
    /// `check_domain_name` validator deliberately skips the reserved-
    /// keyword check that applies to column / table identifiers
    /// — domains live in the type namespace, not the identifier
    /// namespace.
    #[field(domain = "text")]
    pub note: String,
}

fn main() {}
