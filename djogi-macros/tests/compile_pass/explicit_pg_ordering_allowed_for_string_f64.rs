// djogi#372 — `String` and `f64` deliberately RETAIN database-locale
// ordering via `explicit_pg_predicate()` even though neither is
// `DjogiPortableOrd`. The `ExplicitPgOrderable` marker that withholds
// ordering from `Vec<u8>` must NOT regress these. This positive fixture
// fails to compile if `ExplicitPgOrderable` is under-implemented for
// `String` / `f64`.
use djogi::prelude::*;

#[model(table = "metrics")]
#[derive(Debug, Clone)]
pub struct Metric {
    pub name: String,
    pub score: f64,
}

fn _string_explicit_ordering_compiles() {
    let _ = Metric::objects().filter(|f| f.name().explicit_pg_predicate().gt("m".to_string()));
    let _ = Metric::objects()
        .filter(|f| f.name().explicit_pg_predicate().between("a".to_string(), "z".to_string()));
}

fn _f64_explicit_ordering_compiles() {
    let _ = Metric::objects().filter(|f| f.score().explicit_pg_predicate().gt(1.0_f64));
    let _ = Metric::objects().filter(|f| f.score().explicit_pg_predicate().lte(9.5_f64));
}

fn main() {}
