// Phase 8eta PR2d — SQL-only predicates require explicit_pg_predicate route.
//
// `regex` / `iregex` are PostgreSQL-specific predicates (POSIX `~` /
// `~*`). They cannot be evaluated by Sassi/Punnu in memory, so they
// must NOT be reachable from the portable `DjogiField` surface that
// flows through cache and refresh boundaries. Adopters reach them
// through `f.title().explicit_pg_predicate().regex(...)`, which
// returns a legacy `Condition` and is rejected by the cache/refresh
// portability gate as "PostgreSQL-locale, not portable."
//
// This fixture confirms the routing: calling `.regex(...)` directly
// on a `DjogiField` MUST fail to compile because `regex` is
// intentionally not on that receiver. The error span lands on the
// method call, telling the adopter exactly where they need to insert
// `.explicit_pg_predicate()`.
//
// Per `feedback_trybuild_fixtures.md`, every trybuild fixture has
// `fn main` so `.stderr` does not pick up `E0601 (main not found)`.

use djogi::__private::query::__make_djogi_field;
use djogi::prelude::*;
use djogi::query::DjogiField;

#[model(table = "phase8eta_sql_only_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
}

fn main() {
    let name_field: DjogiField<Widget, String> = __make_djogi_field("name", |w| &w.name);

    // `.regex(...)` lives on `ExplicitPgPredicateField<M, String>`
    // only. Calling it directly on `DjogiField<M, String>` must fail
    // to compile because the portable surface deliberately omits
    // PostgreSQL-locale methods.
    let _ = name_field.regex("^rust");
}
