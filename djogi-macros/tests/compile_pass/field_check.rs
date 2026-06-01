// Cluster 2 djogi#105 — `#[field(check = "<sql>")]` attribute.
//
// Exercises the macro's parse + lower path for the new adopter-supplied
// CHECK-constraint attribute:
//
// 1. A simple integer field with a positive-bound CHECK.
// 2. A string field with a non-empty-text CHECK (uses both the column
//    name and a SQL string literal — the expression is emitted verbatim
//    into the column's CHECK constraint).
// 3. A field that already gets a type-derived CHECK (`u32` → BIGINT range)
//    layered with an adopter CHECK. The descriptor carries the adopter
//    expression in `check_sql`; the projection layer combines the two
//    clauses via logical AND into a single constraint slot. Compile-pass
//    verifies the attribute is accepted by darling and reaches descriptor
//    emission without error.
// 4. A FK field with an adopter CHECK — projection still emits the CHECK
//    even though the FK column has no type-derived CHECK.
//
// `no_default` because the surface here does not require user-supplied
// `Default` for the framework-injected columns.

use djogi::prelude::*;

// ── (1) Simple integer CHECK ─────────────────────────────────────────────

#[model(table = "animals_105", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Animal105 {
    pub name: String,
    #[field(check = "weight_kg > 0")]
    pub weight_kg: f64,
}

// ── (2) String / text CHECK with literal ─────────────────────────────────

#[model(table = "tags_105", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Tag105 {
    #[field(check = "char_length(label) > 0")]
    pub label: String,
}

// ── (3) Adopter CHECK combined with a type-derived (u32) CHECK ───────────

#[model(table = "listeners_105", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Listener105 {
    /// `u32` already projects a 0..=4294967295 range CHECK via djogi#190.
    /// The adopter expression below is ANDed with it at projection time.
    #[field(check = "port > 0")]
    pub port: u32,
}

// ── (4) Adopter CHECK on a FK column ─────────────────────────────────────

#[model(table = "owners_105", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Owner105 {
    pub name: String,
}

#[model(table = "vehicles_105", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle105 {
    /// FK columns inherit the parent PK's identity-width type, so no
    /// type-derived CHECK fires. Adopter CHECK still flows through.
    #[field(check = "owner_id > 0")]
    pub owner: ForeignKey<Owner105>,
}

fn main() {}
