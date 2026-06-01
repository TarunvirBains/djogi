#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#216): Piece A references adopter-managed domains; `CREATE DOMAIN` emission is Piece B (deferred). Setup uses raw_ddl to install the domain before sync_models, which is the legitimate adopter-side gap djogi#216 documents.
mod c4_216_domain_live {
    include!("sources/c4_216_domain_live.rs");
}
