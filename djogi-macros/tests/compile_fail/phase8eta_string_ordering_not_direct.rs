// Phase 8eta PR3 — direct ordering on `DjogiField<M, String>` is
// deliberately unavailable.
//
// PostgreSQL text ordering depends on the database's collation, which
// varies across deployments and which Rust's byte-lexicographic `Ord`
// can NOT match for non-ASCII inputs. Allowing a direct `.gt(...)`
// call on a portable string field would let a Punnu in-memory walk
// disagree with the database emit on rows containing `é`, `ß`,
// non-ASCII whitespace, etc. — silently, depending on locale.
//
// PR3 enforces the routing through the type system: direct portable
// ordering is omitted on `DjogiField<M, String>`. Adopters who want
// database-locale ordering reach for
// `f.col().explicit_pg_predicate().gt(...)`, which is rejected at
// cache boundaries (the result is a `Condition` carrying PG-locale
// semantics, not portable to Punnu).
//
// Per `feedback_trybuild_fixtures.md`, every trybuild fixture has
// `fn main` so `.stderr` does not pick up E0601 noise.

use djogi::prelude::*;

#[model(table = "phase8eta_string_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
}

fn main() {
    // Direct portable string ordering is omitted. Adopters who need
    // database-locale ordering route through
    // `f.name().explicit_pg_predicate().gt("m".to_string())` (which
    // is rejected at cache boundaries because PG-locale ordering is
    // not portable to Punnu's byte-lexicographic comparison).
    let _bad = Widget::objects().filter(|f| f.name().gt("m"));
}
