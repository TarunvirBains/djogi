//! `#[djogi_test(extensions = ["postgis"], extensions = ["pg_trgm"])]` — the
//! same `extensions` key specified twice. Sibling case to
//! `djogi_test_sync_models_duplicate_key.rs` (Codex round-2 B-2): both
//! duplicate-key branches share the same parser at testing.rs:288–306, so
//! both need trybuild fixtures to lock the caret column. Without this,
//! a refactor that swaps `Error::new_spanned(meta.path(), ...)` for
//! `meta.span()` or attribute-level spans in the `extensions` branch
//! would degrade the diagnostic without any test surfacing it.

#[djogi::djogi_test(extensions = ["postgis"], extensions = ["pg_trgm"])]
async fn duplicate_extensions_key(mut _ctx: djogi::DjogiContext) {}

fn main() {}
